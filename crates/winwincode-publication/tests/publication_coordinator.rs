// SPDX-License-Identifier: Apache-2.0

use std::collections::HashMap;
use std::fs;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Barrier};
use std::time::{SystemTime, UNIX_EPOCH};

use rusqlite::Connection;
use sha2::{Digest, Sha256};
use winwincode_domain::{
    AttentionItemId, DeliveryId, OrganizationId, ProductSessionId, ProjectId, PublicationId,
    RepositoryId, RequestId, Sha256Digest, UserId, WorkspaceId,
};
use winwincode_publication::{
    MAX_PUBLICATION_DETAIL_HISTORY, PUBLICATION_OPERATION_PROTOCOL,
    PUBLICATION_OPERATION_SCHEMA_VERSION, PublicationAuthorization, PublicationCancelCommand,
    PublicationCommandContext, PublicationEnterpriseAttribution, PublicationErrorKind,
    PublicationFactBinding, PublicationMeteringErrorKind, PublicationMeteringFilter,
    PublicationMeteringLedger, PublicationOperation, PublicationOperationKind,
    PublicationOperationPayload, PublicationPort, PublicationPortError, PublicationPortMutation,
    PublicationPortObservation, PublicationPublishCommand, PublicationReadLedger,
    PublicationResourceFact, PublicationResourceKind, PublicationSourceIssue, PublicationState,
    PublicationTarget, RepositoryPolicyScope,
    test_support::{
        CurrentPublicationCoordinator, current_policy_coordinator, current_publication_fixture,
        current_publication_operations,
    },
};
use winwincode_storage::{ProductStateStorage, SqliteStorage};
use winwincode_storage::{ReceiptActorKey, ReceiptIdentity, ReceiptScopeKey};

#[derive(Default)]
struct RecordingPort {
    resources: HashMap<String, (String, Option<PublicationResourceFact>)>,
    lookups: Vec<String>,
    applies: Vec<String>,
    applied_operations: Vec<PublicationOperation>,
    unknown_after_write_once: Option<PublicationOperationKind>,
    reject_once: Option<PublicationOperationKind>,
    no_remote_write_once: Option<PublicationOperationKind>,
    always_unknown: bool,
}

impl PublicationPort for RecordingPort {
    fn lookup(
        &mut self,
        operation: &PublicationOperation,
    ) -> Result<PublicationPortObservation, PublicationPortError> {
        self.lookups.push(operation.operation_key().to_owned());
        if self.always_unknown {
            return Ok(PublicationPortObservation::unknown(
                operation,
                "provider-unavailable",
            ));
        }
        Ok(self.resources.get(operation.operation_key()).map_or_else(
            || PublicationPortObservation::absent(operation),
            |(request_sha256, resource)| {
                PublicationPortObservation::found(
                    operation,
                    request_sha256.clone(),
                    resource.clone(),
                )
            },
        ))
    }

    fn apply(
        &mut self,
        operation: &PublicationOperation,
    ) -> Result<PublicationPortMutation, PublicationPortError> {
        self.applies.push(operation.operation_key().to_owned());
        self.applied_operations.push(operation.clone());
        if self.reject_once == Some(operation.kind()) {
            self.reject_once = None;
            return Ok(PublicationPortMutation::rejected(
                operation,
                "provider-policy-rejected",
            ));
        }
        let resource = (operation.kind() == PublicationOperationKind::PullRequest).then(|| {
            PublicationResourceFact::try_new(
                PublicationResourceKind::GitHubPullRequest,
                "example/widget",
                42,
            )
            .expect("canonical PR identity")
        });
        self.resources.insert(
            operation.operation_key().to_owned(),
            (operation.request_sha256().to_owned(), resource.clone()),
        );
        if self.unknown_after_write_once == Some(operation.kind()) {
            self.unknown_after_write_once = None;
            return Ok(PublicationPortMutation::unknown(
                operation,
                "provider-response-lost",
            ));
        }
        let remote_write_performed = self.no_remote_write_once != Some(operation.kind());
        if !remote_write_performed {
            self.no_remote_write_once = None;
        }
        Ok(PublicationPortMutation::applied(
            operation,
            resource,
            remote_write_performed,
        ))
    }
}

fn coordinator<'storage, 'port>(
    storage: &'storage mut dyn ProductStateStorage,
    port: &'port mut dyn PublicationPort,
) -> CurrentPublicationCoordinator<'storage, 'port> {
    current_policy_coordinator(storage, port)
}

#[test]
fn unknown_remote_result_is_reconciled_after_restart_without_repeating_the_write() {
    let root = temporary_root();
    let fixture = current_publication_fixture();
    let mut storage = SqliteStorage::open(&root).expect("open storage");
    let mut port = RecordingPort {
        unknown_after_write_once: Some(PublicationOperationKind::Branch),
        ..RecordingPort::default()
    };

    coordinator(&mut storage, &mut port)
        .publish(
            fixture.publish_context(),
            fixture.publish_command(),
            fixture.authorization(),
        )
        .expect("persist publication intent");
    let pending_reconciliation = coordinator(&mut storage, &mut port)
        .resume(fixture.publication_id(), fixture.resume_time_millis())
        .expect("preserve unknown remote result");
    assert_eq!(pending_reconciliation.state(), PublicationState::Publishing);
    assert_eq!(port.applies.len(), 1);

    Box::new(storage).close().expect("close first storage");
    let mut restarted = SqliteStorage::open(&root).expect("reopen storage");
    let published = coordinator(&mut restarted, &mut port)
        .resume(fixture.publication_id(), fixture.resume_time_millis() + 1)
        .expect("reconcile durable publication intent");
    assert_eq!(published.state(), PublicationState::Published);
    assert_eq!(port.applies.len(), 4, "branch must not be applied twice");
    assert_eq!(
        port.applies
            .iter()
            .filter(|key| key.ends_with(":branch"))
            .count(),
        1,
    );
    let metering = metering_entries(&restarted);
    assert_eq!(metering.len(), 4);
    assert!(metering.iter().all(|entry| entry.remote_write_performed));

    Box::new(restarted)
        .close()
        .expect("close restarted storage");
    fs::remove_dir_all(&root).expect("remove fixture");
}

#[test]
fn detail_verifies_every_revision_but_returns_only_the_newest_bounded_history() {
    let root = temporary_root();
    let fixture = current_publication_fixture();
    let mut storage = SqliteStorage::open(&root).expect("open storage");
    let mut port = RecordingPort {
        always_unknown: true,
        ..RecordingPort::default()
    };
    coordinator(&mut storage, &mut port)
        .publish(
            fixture.publish_context(),
            fixture.publish_command(),
            fixture.authorization(),
        )
        .expect("persist publication intent");
    let mut current = None;
    for offset in 0..=MAX_PUBLICATION_DETAIL_HISTORY {
        current = Some(
            coordinator(&mut storage, &mut port)
                .resume(
                    fixture.publication_id(),
                    fixture.resume_time_millis() + u64::try_from(offset).expect("bounded offset"),
                )
                .expect("record a retryable unknown result"),
        );
    }
    let current = current.expect("at least one progress revision");
    let detail = PublicationReadLedger::new(&storage)
        .detail(fixture.publication_id())
        .expect("read bounded verified history");

    assert!(detail.history_truncated());
    assert_eq!(detail.history().len(), MAX_PUBLICATION_DETAIL_HISTORY);
    assert_eq!(detail.publication(), &current);
    assert_eq!(
        detail
            .history()
            .first()
            .expect("first retained revision")
            .revision(),
        current.revision() - u64::try_from(MAX_PUBLICATION_DETAIL_HISTORY - 1).expect("limit")
    );
    assert_eq!(
        detail
            .history()
            .last()
            .expect("current retained revision")
            .revision(),
        current.revision()
    );
    assert!(
        detail
            .history()
            .iter()
            .all(|entry| entry.steps().len() == 4)
    );

    Box::new(storage).close().expect("close storage");
    fs::remove_dir_all(&root).expect("remove fixture");
}

fn temporary_root() -> PathBuf {
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let nonce = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("clock")
        .as_nanos();
    std::env::temp_dir().join(format!(
        "winwincode-publication-{}-{nonce}-{}",
        std::process::id(),
        COUNTER.fetch_add(1, Ordering::Relaxed),
    ))
}

#[test]
fn delivery_target_digest_and_human_actor_seal_one_current_publication_authority() {
    let target = PublicationTarget::try_github(
        "example/widget",
        "main",
        "example/widget",
        "winwincode/delivery",
    )
    .expect("canonical target");
    let binding = PublicationFactBinding::try_new(
        DeliveryId("dlv_00000000000000000000000001".to_owned()),
        21,
        "spec_00000000000000000000000001",
        1,
        format!("git-candidate:sha256:{}", "a".repeat(64)),
        "c".repeat(64),
        "verdict:fixture:pass",
        AttentionItemId("att_00000000000000000000000001".to_owned()),
        "d".repeat(64),
        "57f0168ebca2edeeeee513b8bf628462f71052158e3cf1ecde2bce27268b5774",
    )
    .expect("Control Plane publication binding");

    let authorization = PublicationAuthorization::try_from_current_facts(
        binding,
        PublicationSourceIssue::try_github("example/widget", 7).expect("source issue"),
        target.clone(),
        "a".repeat(40),
        "art_00000000000000000000000001",
        Sha256Digest(format!("sha256:{}", "b".repeat(64))),
        "usr_00000000000000000000000001",
        1_000,
        Sha256Digest(format!("sha256:{}", "f".repeat(64))),
    )
    .expect("the exact Delivery target must seal without another target representation");

    assert_eq!(authorization.target(), &target);

    let non_human = PublicationAuthorization::try_from_current_facts(
        authorization.binding().clone(),
        authorization.source().clone(),
        target,
        authorization.candidate_commit_id(),
        authorization.artifact_id(),
        authorization.artifact_digest().clone(),
        "svc_00000000000000000000000001",
        authorization.approved_at_millis(),
        authorization.repository_scope_sha256().clone(),
    );
    assert!(
        non_human.is_err(),
        "a service identity is not the required human publication approval"
    );
}

#[test]
fn exact_approved_publication_persists_intent_then_converges_without_duplicate_writes() {
    let root = temporary_root();
    let fixture = current_publication_fixture();
    let mut storage = SqliteStorage::open(&root).expect("open storage");
    let mut port = RecordingPort::default();

    let pending = coordinator(&mut storage, &mut port)
        .publish(
            fixture.publish_context(),
            fixture.publish_command(),
            fixture.authorization(),
        )
        .expect("persist publication intent");
    assert_eq!(pending.state(), PublicationState::Pending);
    assert_eq!(pending.revision(), 1);
    assert!(port.lookups.is_empty());
    assert!(port.applies.is_empty());

    let replay = coordinator(&mut storage, &mut port)
        .publish(
            fixture.publish_context(),
            fixture.publish_command(),
            fixture.authorization(),
        )
        .expect("replay exact publish command");
    assert_eq!(replay, pending);
    assert!(port.applies.is_empty());

    let published = coordinator(&mut storage, &mut port)
        .resume(fixture.publication_id(), fixture.resume_time_millis())
        .expect("publish every durable operation");
    assert_eq!(published.state(), PublicationState::Published);
    assert_eq!(port.applies.len(), 4);
    assert_eq!(port.lookups.len(), 4);
    assert_eq!(
        published.resource().expect("published PR").kind(),
        PublicationResourceKind::GitHubPullRequest,
    );
    let result = published
        .result_fact()
        .expect("projection-safe result fact");
    assert_eq!(result.state(), "published");
    assert_eq!(result.binding(), fixture.authorization().binding());
    assert_eq!(
        result.publication_set_sha256(),
        fixture.authorization().publication_set_sha256(),
    );
    assert_eq!(result.resource(), published.resource());
    assert!(port.applied_operations.iter().all(|operation| {
        operation.schema_version() == PUBLICATION_OPERATION_SCHEMA_VERSION
            && operation.protocol() == PUBLICATION_OPERATION_PROTOCOL
    }));
    assert!(matches!(
        port.applied_operations[0].payload(),
        PublicationOperationPayload::Branch {
            repository,
            branch,
            commit_id,
        } if repository == "example/widget"
            && branch == "winwincode/delivery"
            && commit_id == &"a".repeat(40)
    ));
    let metering = metering_entries(&storage);
    assert_eq!(metering.len(), 4);
    assert!(metering.iter().all(|entry| {
        entry.publication_id == *fixture.publication_id()
            && entry.remote_write_performed
            && entry.attribution == *fixture.attribution()
            && entry.request_sha256.starts_with("sha256:")
    }));

    let converged = coordinator(&mut storage, &mut port)
        .resume(fixture.publication_id(), fixture.resume_time_millis() + 1)
        .expect("published result is stable");
    assert_eq!(converged, published);
    assert_eq!(port.applies.len(), 4);
    assert_eq!(port.lookups.len(), 4);

    Box::new(storage).close().expect("close storage");
    fs::remove_dir_all(&root).expect("remove fixture");
}

#[test]
fn cancel_after_partial_remote_progress_changes_only_the_publication_and_replays_exactly() {
    let root = temporary_root();
    let fixture = current_publication_fixture();
    let mut storage = SqliteStorage::open(&root).expect("open storage");
    let mut port = RecordingPort {
        unknown_after_write_once: Some(PublicationOperationKind::Branch),
        ..RecordingPort::default()
    };

    let initial = coordinator(&mut storage, &mut port)
        .publish(
            fixture.publish_context(),
            fixture.publish_command(),
            fixture.authorization(),
        )
        .expect("persist publication intent");
    let partial = coordinator(&mut storage, &mut port)
        .resume(fixture.publication_id(), fixture.resume_time_millis())
        .expect("preserve partial publication");
    assert_eq!(partial.state(), PublicationState::Publishing);
    let port_calls_before_cancel = (port.lookups.len(), port.applies.len());

    let command = PublicationCancelCommand::try_new(
        fixture.publication_id().clone(),
        "operator cancelled this publication",
    )
    .expect("canonical cancellation");
    let context = PublicationCommandContext::try_new(
        ReceiptIdentity::new(
            ReceiptActorKey::from_encoded(b"fixture-publication-actor".to_vec())
                .expect("actor key"),
            ReceiptScopeKey::from_encoded(b"fixture-publication-repository-scope".to_vec())
                .expect("scope key"),
            RequestId("req_00000000000000000000000002".to_owned()),
        )
        .expect("receipt identity"),
        Sha256Digest(format!("sha256:{}", "1".repeat(64))),
        partial.revision(),
        fixture.resume_time_millis() + 1,
    )
    .expect("cancel context");
    let cancelled = coordinator(&mut storage, &mut port)
        .cancel(&context, &command)
        .expect("cancel publication");
    assert_eq!(cancelled.state(), PublicationState::Cancelled);
    assert_eq!(cancelled.revision(), partial.revision() + 1);
    assert_eq!(cancelled.binding(), initial.binding());
    assert_eq!(
        (port.lookups.len(), port.applies.len()),
        port_calls_before_cancel,
    );
    assert!(metering_entries(&storage).is_empty());

    let replay = coordinator(&mut storage, &mut port)
        .cancel(&context, &command)
        .expect("replay exact cancellation");
    assert_eq!(replay, cancelled);
    let terminal = coordinator(&mut storage, &mut port)
        .resume(fixture.publication_id(), fixture.resume_time_millis() + 2)
        .expect("cancelled publication remains terminal");
    assert_eq!(terminal, cancelled);
    assert_eq!(
        (port.lookups.len(), port.applies.len()),
        port_calls_before_cancel,
    );

    Box::new(storage).close().expect("close storage");
    fs::remove_dir_all(&root).expect("remove fixture");
}

#[test]
fn current_read_rejects_a_valid_looking_state_that_no_longer_matches_the_durable_intent() {
    let root = temporary_root();
    let fixture = current_publication_fixture();
    let mut storage = SqliteStorage::open(&root).expect("open storage");
    let mut port = RecordingPort::default();
    coordinator(&mut storage, &mut port)
        .publish(
            fixture.publish_context(),
            fixture.publish_command(),
            fixture.authorization(),
        )
        .expect("persist publication intent");
    Box::new(storage).close().expect("close storage");

    let database = root.join("control-plane.sqlite3");
    let connection = Connection::open(&database).expect("open raw database");
    let payload: Vec<u8> = connection
        .query_row(
            "SELECT payload FROM product_state WHERE stream_id = ?1",
            [format!("publication:{}", fixture.publication_id().0)],
            |row| row.get(0),
        )
        .expect("read publication state");
    let mut value: serde_json::Value = serde_json::from_slice(&payload).expect("state JSON");
    value["target"]["repository"] = serde_json::Value::String("foreign/widget".to_owned());
    connection
        .execute(
            "UPDATE product_state SET payload = ?1 WHERE stream_id = ?2",
            (
                serde_json::to_vec(&value).expect("encode modified state"),
                format!("publication:{}", fixture.publication_id().0),
            ),
        )
        .expect("modify publication state");
    drop(connection);

    let mut reopened = SqliteStorage::open(&root).expect("reopen storage");
    let error = coordinator(&mut reopened, &mut port)
        .get(fixture.publication_id())
        .expect_err("state must remain bound to its durable intent and journal");
    assert_eq!(error.kind(), PublicationErrorKind::Corrupt);

    Box::new(reopened).close().expect("close reopened storage");
    fs::remove_dir_all(&root).expect("remove fixture");
}

#[test]
fn current_read_rejects_step_metadata_that_claims_progress_without_a_step_transition() {
    let root = temporary_root();
    let fixture = current_publication_fixture();
    let mut storage = SqliteStorage::open(&root).expect("open storage");
    let mut port = RecordingPort::default();
    coordinator(&mut storage, &mut port)
        .publish(
            fixture.publish_context(),
            fixture.publish_command(),
            fixture.authorization(),
        )
        .expect("persist publication intent");
    Box::new(storage).close().expect("close storage");

    let connection = Connection::open(root.join("control-plane.sqlite3")).expect("open database");
    let stream_id = format!("publication:{}", fixture.publication_id().0);
    let payload: Vec<u8> = connection
        .query_row(
            "SELECT payload FROM product_state WHERE stream_id = ?1",
            [&stream_id],
            |row| row.get(0),
        )
        .expect("read publication state");
    let mut value: serde_json::Value = serde_json::from_slice(&payload).expect("state JSON");
    value["steps"][0]["code"] = serde_json::Value::String("fabricated-progress".to_owned());
    let modified = serde_json::to_vec(&value).expect("encode modified state");
    let digest = format!("sha256:{:x}", Sha256::digest(&modified));
    connection
        .execute(
            "UPDATE product_state SET payload = ?1 WHERE stream_id = ?2",
            (&modified, &stream_id),
        )
        .expect("modify publication state");
    connection
        .execute(
            "UPDATE aggregate_journal_records SET payload = ?1, digest = ?2 \
             WHERE aggregate_type = 'publication' AND aggregate_id = ?3 AND sequence = 1",
            (&modified, &digest, &fixture.publication_id().0),
        )
        .expect("modify publication journal tail");
    drop(connection);

    let mut reopened = SqliteStorage::open(&root).expect("reopen storage");
    let error = coordinator(&mut reopened, &mut port)
        .get(fixture.publication_id())
        .expect_err("step metadata without a matching transition must be rejected");
    assert_eq!(error.kind(), PublicationErrorKind::Corrupt,);

    Box::new(reopened).close().expect("close reopened storage");
    fs::remove_dir_all(&root).expect("remove fixture");
}

#[test]
fn current_read_rejects_individually_valid_snapshots_with_an_impossible_history_transition() {
    let root = temporary_root();
    let fixture = current_publication_fixture();
    let mut storage = SqliteStorage::open(&root).expect("open storage");
    let mut port = RecordingPort {
        unknown_after_write_once: Some(PublicationOperationKind::Branch),
        ..RecordingPort::default()
    };
    coordinator(&mut storage, &mut port)
        .publish(
            fixture.publish_context(),
            fixture.publish_command(),
            fixture.authorization(),
        )
        .expect("persist publication intent");
    coordinator(&mut storage, &mut port)
        .resume(fixture.publication_id(), fixture.resume_time_millis())
        .expect("record several valid progress revisions");
    Box::new(storage).close().expect("close storage");

    let connection = Connection::open(root.join("control-plane.sqlite3")).expect("open database");
    let payload: Vec<u8> = connection
        .query_row(
            "SELECT payload FROM aggregate_journal_records \
             WHERE aggregate_type = 'publication' AND aggregate_id = ?1 AND sequence = 2",
            [fixture.publication_id().0.clone()],
            |row| row.get(0),
        )
        .expect("read second publication revision");
    let mut value: serde_json::Value = serde_json::from_slice(&payload).expect("state JSON");
    value["state"] = serde_json::Value::String("pending".to_owned());
    let modified = serde_json::to_vec(&value).expect("encode individually valid snapshot");
    let digest = format!("sha256:{:x}", Sha256::digest(&modified));
    connection
        .execute(
            "UPDATE aggregate_journal_records SET payload = ?1, digest = ?2 \
             WHERE aggregate_type = 'publication' AND aggregate_id = ?3 AND sequence = 2",
            (&modified, &digest, &fixture.publication_id().0),
        )
        .expect("replace one historical snapshot");
    drop(connection);

    let mut reopened = SqliteStorage::open(&root).expect("reopen storage");
    let error = coordinator(&mut reopened, &mut port)
        .get(fixture.publication_id())
        .expect_err("an impossible publication history transition must fail closed");
    assert_eq!(error.kind(), PublicationErrorKind::Corrupt);

    Box::new(reopened).close().expect("close reopened storage");
    fs::remove_dir_all(&root).expect("remove fixture");
}

#[test]
fn exact_publish_replay_returns_the_receipt_before_reading_corrupt_current_facts() {
    let root = temporary_root();
    let fixture = current_publication_fixture();
    let mut storage = SqliteStorage::open(&root).expect("open storage");
    let mut port = RecordingPort::default();
    let original = coordinator(&mut storage, &mut port)
        .publish(
            fixture.publish_context(),
            fixture.publish_command(),
            fixture.authorization(),
        )
        .expect("persist publication intent");
    Box::new(storage).close().expect("close storage");

    let connection = Connection::open(root.join("control-plane.sqlite3")).expect("open database");
    connection
        .execute(
            "UPDATE product_state SET payload = X'00' WHERE stream_id = ?1",
            [format!("publication:{}", fixture.publication_id().0)],
        )
        .expect("corrupt publication state");
    connection
        .execute(
            "UPDATE aggregate_journal_records SET payload = X'00' \
             WHERE aggregate_type = 'publication' AND aggregate_id = ?1",
            [fixture.publication_id().0.clone()],
        )
        .expect("corrupt publication journal");
    drop(connection);

    let mut reopened = SqliteStorage::open(&root).expect("reopen storage");
    let replay = coordinator(&mut reopened, &mut port)
        .publish(
            fixture.publish_context(),
            fixture.publish_command(),
            fixture.authorization(),
        )
        .expect("return the original durable receipt");
    assert_eq!(replay, original);
    assert!(port.lookups.is_empty());
    assert!(port.applies.is_empty());

    Box::new(reopened).close().expect("close reopened storage");
    fs::remove_dir_all(&root).expect("remove fixture");
}

#[test]
fn stale_candidate_command_is_rejected_before_intent_or_provider_activity() {
    let root = temporary_root();
    let fixture = current_publication_fixture();
    let mut storage = SqliteStorage::open(&root).expect("open storage");
    let mut port = RecordingPort::default();
    let stale = PublicationPublishCommand::try_new(
        fixture.publication_id().clone(),
        fixture.authorization().binding().delivery_id().clone(),
        Sha256Digest(format!("sha256:{}", "0".repeat(64))),
        fixture.authorization().target().clone(),
    )
    .expect("well-shaped stale command");

    let error = coordinator(&mut storage, &mut port)
        .publish(fixture.publish_context(), &stale, fixture.authorization())
        .expect_err("stale candidate cannot publish");
    assert_eq!(error.kind(), PublicationErrorKind::StaleAuthority,);
    let missing = coordinator(&mut storage, &mut port)
        .get(fixture.publication_id())
        .expect_err("stale command must not persist intent");
    assert_eq!(missing.kind(), PublicationErrorKind::NotFound,);
    assert!(port.lookups.is_empty());
    assert!(port.applies.is_empty());

    Box::new(storage).close().expect("close storage");
    fs::remove_dir_all(&root).expect("remove fixture");
}

#[test]
fn rejected_remote_step_fails_without_calling_later_operations() {
    let root = temporary_root();
    let fixture = current_publication_fixture();
    let mut storage = SqliteStorage::open(&root).expect("open storage");
    let mut port = RecordingPort {
        reject_once: Some(PublicationOperationKind::PullRequest),
        ..RecordingPort::default()
    };
    coordinator(&mut storage, &mut port)
        .publish(
            fixture.publish_context(),
            fixture.publish_command(),
            fixture.authorization(),
        )
        .expect("persist publication intent");

    let failed = coordinator(&mut storage, &mut port)
        .resume(fixture.publication_id(), fixture.resume_time_millis())
        .expect("record exact rejection");
    assert_eq!(failed.state(), PublicationState::Failed);
    assert_eq!(port.applies.len(), 2);
    assert!(port.applies[0].ends_with(":branch"));
    assert!(port.applies[1].ends_with(":pull-request"));
    let metering = metering_entries(&storage);
    assert_eq!(metering.len(), 1);
    assert!(metering[0].operation_key.ends_with(":branch"));
    let calls = (port.lookups.len(), port.applies.len());
    let terminal = coordinator(&mut storage, &mut port)
        .resume(fixture.publication_id(), fixture.resume_time_millis() + 1)
        .expect("failed publication remains terminal");
    assert_eq!(terminal, failed);
    assert_eq!((port.lookups.len(), port.applies.len()), calls);

    Box::new(storage).close().expect("close storage");
    fs::remove_dir_all(&root).expect("remove fixture");
}

#[test]
fn lookup_found_writes_metering_source_while_applied_without_remote_write_does_not() {
    let root = temporary_root();
    let fixture = current_publication_fixture();
    let operations = current_publication_operations();
    let mut storage = SqliteStorage::open(&root).expect("open storage");
    let mut found_port = RecordingPort::default();
    for operation in &operations {
        let resource = (operation.kind() == PublicationOperationKind::PullRequest).then(|| {
            PublicationResourceFact::try_new(
                PublicationResourceKind::GitHubPullRequest,
                "example/widget",
                42,
            )
            .expect("canonical PR identity")
        });
        found_port.resources.insert(
            operation.operation_key().to_owned(),
            (operation.request_sha256().to_owned(), resource),
        );
    }
    coordinator(&mut storage, &mut found_port)
        .publish(
            fixture.publish_context(),
            fixture.publish_command(),
            fixture.authorization(),
        )
        .expect("persist lookup-only publication");
    let published = coordinator(&mut storage, &mut found_port)
        .resume(fixture.publication_id(), fixture.resume_time_millis())
        .expect("converge from provider lookup facts");
    assert_eq!(published.state(), PublicationState::Published);
    assert!(found_port.applies.is_empty());
    let found_metering = metering_entries(&storage);
    assert_eq!(found_metering.len(), 4);
    assert!(
        found_metering
            .iter()
            .all(|entry| entry.remote_write_performed)
    );
    Box::new(storage).close().expect("close found storage");

    let false_root = temporary_root();
    let fixture = current_publication_fixture();
    let mut storage = SqliteStorage::open(&false_root).expect("open false-write storage");
    let mut port = RecordingPort {
        no_remote_write_once: Some(PublicationOperationKind::Branch),
        ..RecordingPort::default()
    };
    coordinator(&mut storage, &mut port)
        .publish(
            fixture.publish_context(),
            fixture.publish_command(),
            fixture.authorization(),
        )
        .expect("persist publication");
    coordinator(&mut storage, &mut port)
        .resume(fixture.publication_id(), fixture.resume_time_millis())
        .expect("converge with one no-write apply result");
    let metering = metering_entries(&storage);
    assert_eq!(metering.len(), 3);
    assert!(
        metering
            .iter()
            .all(|entry| !entry.operation_key.ends_with(":branch")),
    );
    Box::new(storage)
        .close()
        .expect("close false-write storage");

    fs::remove_dir_all(root).expect("remove found fixture");
    fs::remove_dir_all(false_root).expect("remove false-write fixture");
}

fn metering_entries(
    storage: &dyn ProductStateStorage,
) -> Vec<winwincode_publication::PublicationMeteringSourceEntry> {
    let page = PublicationMeteringLedger::new(storage)
        .scan_sources(&PublicationMeteringFilter::default(), None, 200)
        .expect("scan Publication metering sources");
    assert!(page.next.is_none());
    page.entries
}

#[test]
fn request_identity_conflict_and_duplicate_publication_are_distinct_and_write_nothing() {
    let root = temporary_root();
    let fixture = current_publication_fixture();
    let mut storage = SqliteStorage::open(&root).expect("open storage");
    let mut port = RecordingPort::default();
    let original = coordinator(&mut storage, &mut port)
        .publish(
            fixture.publish_context(),
            fixture.publish_command(),
            fixture.authorization(),
        )
        .expect("persist publication intent");

    let changed_body = PublicationCommandContext::try_new(
        fixture.publish_context().receipt_identity().clone(),
        Sha256Digest(format!("sha256:{}", "2".repeat(64))),
        0,
        fixture.publish_context().occurred_at_millis(),
    )
    .expect("changed command digest");
    let conflict = coordinator(&mut storage, &mut port)
        .publish(
            &changed_body,
            fixture.publish_command(),
            fixture.authorization(),
        )
        .expect_err("same request identity cannot change body");
    assert_eq!(conflict.kind(), PublicationErrorKind::RequestConflict,);

    let different_request = PublicationCommandContext::try_new(
        ReceiptIdentity::new(
            ReceiptActorKey::from_encoded(b"fixture-publication-actor".to_vec())
                .expect("actor key"),
            ReceiptScopeKey::from_encoded(b"fixture-publication-repository-scope".to_vec())
                .expect("scope key"),
            RequestId("req_00000000000000000000000003".to_owned()),
        )
        .expect("different request identity"),
        Sha256Digest(format!("sha256:{}", "3".repeat(64))),
        0,
        fixture.publish_context().occurred_at_millis(),
    )
    .expect("different request context");
    let duplicate = coordinator(&mut storage, &mut port)
        .publish(
            &different_request,
            fixture.publish_command(),
            fixture.authorization(),
        )
        .expect_err("another request cannot recreate one publication identity");
    assert_eq!(duplicate.kind(), PublicationErrorKind::AlreadyExists,);
    assert_eq!(
        coordinator(&mut storage, &mut port)
            .get(fixture.publication_id())
            .expect("read original publication"),
        original,
    );
    assert!(port.lookups.is_empty());
    assert!(port.applies.is_empty());

    Box::new(storage).close().expect("close storage");
    fs::remove_dir_all(&root).expect("remove fixture");
}

#[test]
fn exact_replay_rejects_changed_product_session_user_and_scope_attribution() {
    let root = temporary_root();
    let fixture = current_publication_fixture();
    let mut storage = SqliteStorage::open(&root).expect("open storage");
    let mut port = RecordingPort::default();
    coordinator(&mut storage, &mut port)
        .publish(
            fixture.publish_context(),
            fixture.publish_command(),
            fixture.authorization(),
        )
        .expect("persist Publication attribution");

    let exact_scope = RepositoryPolicyScope::try_new(
        OrganizationId("org_00000000000000000000000001".to_owned()),
        WorkspaceId("wsp_00000000000000000000000001".to_owned()),
        ProjectId("prj_00000000000000000000000001".to_owned()),
        RepositoryId("rep_00000000000000000000000001".to_owned()),
    )
    .expect("exact fixture scope");
    let variants = [
        PublicationEnterpriseAttribution::try_new(
            &exact_scope,
            fixture.authorization().binding().delivery_id().clone(),
            ProductSessionId("psn_00000000000000000000000002".to_owned()),
            fixture.attribution().user_id().clone(),
        )
        .expect("changed ProductSession attribution"),
        PublicationEnterpriseAttribution::try_new(
            &exact_scope,
            fixture.authorization().binding().delivery_id().clone(),
            fixture.attribution().product_session_id().clone(),
            UserId("usr_00000000000000000000000003".to_owned()),
        )
        .expect("changed User attribution"),
        PublicationEnterpriseAttribution::try_new(
            &RepositoryPolicyScope::try_new(
                OrganizationId("org_00000000000000000000000002".to_owned()),
                exact_scope.workspace_id().clone(),
                exact_scope.project_id().clone(),
                exact_scope.repository_id().clone(),
            )
            .expect("changed organization scope"),
            fixture.authorization().binding().delivery_id().clone(),
            fixture.attribution().product_session_id().clone(),
            fixture.attribution().user_id().clone(),
        )
        .expect("changed scope attribution"),
    ];
    for changed in variants {
        let error = coordinator(&mut storage, &mut port)
            .publish_with_attribution(
                fixture.publish_context(),
                fixture.publish_command(),
                fixture.authorization(),
                &changed,
            )
            .expect_err("changed durable attribution must fail closed");
        assert_eq!(error.kind(), PublicationErrorKind::Corrupt);
    }
    assert!(port.lookups.is_empty());
    assert!(port.applies.is_empty());

    Box::new(storage).close().expect("close storage");
    fs::remove_dir_all(root).expect("remove fixture");
}

#[test]
fn metering_source_scan_keeps_a_bounded_filter_bound_snapshot_across_restart() {
    let root = temporary_root();
    let fixture = current_publication_fixture();
    let mut storage = SqliteStorage::open(&root).expect("open storage");
    let mut first_port = RecordingPort::default();
    coordinator(&mut storage, &mut first_port)
        .publish(
            fixture.publish_context(),
            fixture.publish_command(),
            fixture.authorization(),
        )
        .expect("persist first Publication");
    coordinator(&mut storage, &mut first_port)
        .resume(fixture.publication_id(), fixture.resume_time_millis())
        .expect("settle first Publication");

    let filter = PublicationMeteringFilter {
        user_id: Some(fixture.attribution().user_id().clone()),
        ..PublicationMeteringFilter::default()
    };
    let first_page = PublicationMeteringLedger::new(&storage)
        .scan_sources(&filter, None, 2)
        .expect("scan first fixed-snapshot page");
    assert_eq!(first_page.snapshot_sequence, 4);
    assert_eq!(first_page.entries.len(), 2);
    let cursor = first_page.next.expect("remaining first snapshot page");

    let second_publication = PublicationId("pub_00000000000000000000000002".to_owned());
    let second_command = PublicationPublishCommand::try_new(
        second_publication.clone(),
        fixture.authorization().binding().delivery_id().clone(),
        fixture.authorization().candidate_digest().clone(),
        fixture.authorization().target().clone(),
    )
    .expect("second Publication command");
    let second_context = PublicationCommandContext::try_new(
        ReceiptIdentity::new(
            ReceiptActorKey::from_encoded(b"fixture-publication-actor".to_vec())
                .expect("actor key"),
            ReceiptScopeKey::from_encoded(b"fixture-publication-repository-scope".to_vec())
                .expect("scope key"),
            RequestId("req_00000000000000000000000004".to_owned()),
        )
        .expect("second receipt identity"),
        Sha256Digest(format!("sha256:{}", "4".repeat(64))),
        0,
        fixture.publish_context().occurred_at_millis() + 1,
    )
    .expect("second command context");
    let mut second_port = RecordingPort::default();
    coordinator(&mut storage, &mut second_port)
        .publish_with_attribution(
            &second_context,
            &second_command,
            fixture.authorization(),
            fixture.attribution(),
        )
        .expect("persist second Publication");
    coordinator(&mut storage, &mut second_port)
        .resume(&second_publication, fixture.resume_time_millis() + 1)
        .expect("settle second Publication");
    assert_eq!(metering_entries(&storage).len(), 8);

    Box::new(storage).close().expect("close first storage");
    let restarted = SqliteStorage::open(&root).expect("restart storage");
    let remaining = PublicationMeteringLedger::new(&restarted)
        .scan_sources(&filter, Some(&cursor), 2)
        .expect("continue the original bounded snapshot after restart");
    assert_eq!(remaining.snapshot_sequence, 4);
    assert_eq!(remaining.entries.len(), 2);
    assert!(remaining.next.is_none());
    assert!(
        remaining
            .entries
            .iter()
            .all(|entry| entry.publication_id == *fixture.publication_id()),
    );
    let changed_filter = PublicationMeteringFilter {
        publication_id: Some(second_publication),
        ..filter
    };
    let error = PublicationMeteringLedger::new(&restarted)
        .scan_sources(&changed_filter, Some(&cursor), 2)
        .expect_err("a cursor cannot be reused with a changed filter");
    assert_eq!(error.kind(), PublicationMeteringErrorKind::InvalidInput,);

    Box::new(restarted)
        .close()
        .expect("close restarted storage");
    fs::remove_dir_all(root).expect("remove fixture");
}

#[test]
fn concurrent_exact_publish_requests_create_one_intent_and_one_remote_effect_set() {
    const CALLERS: usize = 8;
    let root = temporary_root();
    let fixture = current_publication_fixture();
    let command = fixture.publish_command().clone();
    let context = fixture.publish_context().clone();
    let authorization = fixture.authorization().clone();
    let publication_id = fixture.publication_id().clone();
    let resume_time = fixture.resume_time_millis();
    Box::new(SqliteStorage::open(&root).expect("initialize storage"))
        .close()
        .expect("close initialized storage");

    let barrier = Arc::new(Barrier::new(CALLERS));
    let callers = (0..CALLERS)
        .map(|_| {
            let root = root.clone();
            let barrier = Arc::clone(&barrier);
            let command = command.clone();
            let context = context.clone();
            let authorization = authorization.clone();
            std::thread::spawn(move || {
                let mut storage = SqliteStorage::open(&root).expect("open concurrent storage");
                let mut port = RecordingPort::default();
                barrier.wait();
                let result = coordinator(&mut storage, &mut port)
                    .publish(&context, &command, &authorization)
                    .expect("exact concurrent request converges");
                assert!(port.lookups.is_empty());
                assert!(port.applies.is_empty());
                Box::new(storage).close().expect("close concurrent storage");
                result
            })
        })
        .collect::<Vec<_>>();
    let results = callers
        .into_iter()
        .map(|caller| caller.join().expect("concurrent caller"))
        .collect::<Vec<_>>();
    assert!(results.iter().all(|result| result == &results[0]));

    let connection = Connection::open(root.join("control-plane.sqlite3")).expect("open database");
    for (table, expected) in [
        ("product_state", 2_i64),
        ("aggregate_journals", 1),
        ("aggregate_journal_records", 1),
        ("command_receipts", 1),
        ("outbox", 1),
    ] {
        let count: i64 = connection
            .query_row(&format!("SELECT COUNT(*) FROM {table}"), [], |row| {
                row.get(0)
            })
            .expect("count durable publication facts");
        assert_eq!(count, expected, "{table}");
    }
    drop(connection);

    let mut storage = SqliteStorage::open(&root).expect("reopen storage");
    let mut port = RecordingPort::default();
    let published = coordinator(&mut storage, &mut port)
        .resume(&publication_id, resume_time)
        .expect("apply one canonical remote effect set");
    assert_eq!(published.state(), PublicationState::Published);
    assert_eq!(port.applies.len(), 4);
    assert_eq!(
        port.applies
            .iter()
            .collect::<std::collections::HashSet<_>>()
            .len(),
        4
    );

    Box::new(storage).close().expect("close storage");
    fs::remove_dir_all(&root).expect("remove fixture");
}
