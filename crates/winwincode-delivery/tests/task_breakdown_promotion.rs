use std::sync::Arc;

use serde::Serialize;
use sha2::{Digest, Sha256};
use winwincode_delivery::{
    domain::{Delivery, DeliveryTaskStatus},
    store::{
        AppendDelivery, ApproveDeliveryTaskBreakdown, AtomicPublication,
        DELIVERY_STORE_SCHEMA_VERSION, DeliveryCommand, DeliveryCommandPort, DeliveryJournalCodec,
        DeliveryJournalPort, DeliveryMutationOperation, DeliveryQuery, DeliveryQueryPort,
        DeliveryStore, DeliveryStoreErrorCode, DeliveryStoreManifest, DeliveryStoreRecord,
        InMemoryDeliveryJournal, JournalEntryState, JournalRecordBytes,
    },
};
use winwincode_domain::{DeliveryId, RequestId};

const REVIEW_SET_SHA256: &str = "06123389bf88cb8915e399fdb2baccc9460d836de763bccdea3effd7084435e3";

fn approved_delivery() -> Delivery {
    Delivery::decode_json(include_bytes!(
        "fixtures/delivery-approved-solution-review.json"
    ))
    .expect("approved solution-review fixture")
}

fn seeded_store() -> DeliveryStore<'static> {
    let delivery = approved_delivery();
    let journal = Arc::new(InMemoryDeliveryJournal::new());
    seed_journal(journal.as_ref(), &delivery, "seed-task-breakdown");
    DeliveryStore::new(journal)
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct RecordPayload<'delivery> {
    schema_version: u8,
    delivery_id: &'delivery DeliveryId,
    sequence: &'delivery str,
    request_id: &'delivery RequestId,
    request_digest: &'delivery str,
    operation: DeliveryMutationOperation,
    previous_digest: Option<&'delivery str>,
    snapshot: &'delivery Delivery,
}

fn seed_journal(journal: &InMemoryDeliveryJournal, delivery: &Delivery, request: &str) {
    let request_id = RequestId(request.into());
    let request_digest = "a".repeat(64);
    let sequence = "1";
    let digest = format!(
        "{:x}",
        Sha256::digest(
            serde_json::to_vec(&RecordPayload {
                schema_version: DELIVERY_STORE_SCHEMA_VERSION,
                delivery_id: delivery.id(),
                sequence,
                request_id: &request_id,
                request_digest: &request_digest,
                operation: DeliveryMutationOperation::DeliveryCreated,
                previous_digest: None,
                snapshot: delivery,
            })
            .expect("record payload"),
        )
    );
    let record = DeliveryStoreRecord {
        schema_version: DELIVERY_STORE_SCHEMA_VERSION,
        delivery_id: delivery.id().clone(),
        sequence: sequence.into(),
        request_id,
        request_digest,
        operation: DeliveryMutationOperation::DeliveryCreated,
        previous_digest: None,
        snapshot: delivery.clone(),
        digest: digest.clone(),
    };
    let manifest = DeliveryStoreManifest {
        schema_version: DELIVERY_STORE_SCHEMA_VERSION,
        delivery_id: delivery.id().clone(),
        created_at_millis: delivery.snapshot().created_at_millis,
        first_record_digest: digest.clone(),
    };
    journal
        .publish(AtomicPublication::Create {
            delivery_id: delivery.id().clone(),
            manifest: DeliveryJournalCodec::encode_manifest(&manifest).expect("manifest"),
            first_record: JournalRecordBytes {
                sequence: 1,
                state: JournalEntryState::Published,
                digest,
                bytes: DeliveryJournalCodec::encode_record(&record).expect("record"),
            },
        })
        .expect("seed approved journal");
}

fn approval(request_id: &str, expected_revision: u64) -> DeliveryCommand {
    DeliveryCommand::ApproveTaskBreakdown(Box::new(ApproveDeliveryTaskBreakdown {
        delivery_id: DeliveryId("delivery-main".into()),
        request_id: RequestId(request_id.into()),
        request_digest: "b".repeat(64),
        expected_revision,
        review_set_sha256: REVIEW_SET_SHA256.into(),
    }))
}

#[test]
fn task_breakdown_store_promotes_the_exact_ordered_graph_once() {
    let store = seeded_store();
    let result = store
        .execute(approval("approve-task-breakdown", 1))
        .expect("DeliveryCommand::ApproveTaskBreakdown");
    let tasks = &result.snapshot.snapshot().tasks;

    assert_eq!(tasks.len(), 1);
    assert_eq!(tasks[0].id.0, "task:invitation");
    assert_eq!(tasks[0].title, "Implement invitation flow");
    assert_eq!(tasks[0].goal, "Deliver every current acceptance criterion.");
    assert_eq!(tasks[0].owner, None);
    assert_eq!(tasks[0].status, DeliveryTaskStatus::Pending);
    assert_eq!(
        result
            .task_breakdown_event
            .as_ref()
            .expect("task event")
            .review_set_sha256,
        REVIEW_SET_SHA256
    );

    let second = store
        .execute(approval("approve-task-breakdown-second", 2))
        .expect_err("the exact review_set_sha256 cannot promote a second graph");
    assert_eq!(second.code(), DeliveryStoreErrorCode::InvalidStoreOptions);
}

#[test]
fn task_breakdown_store_rejects_stale_foreign_revised_or_changed_review() {
    let stale_store = seeded_store();
    let mut stale = approval("stale-task-breakdown", 1);
    let DeliveryCommand::ApproveTaskBreakdown(stale_command) = &mut stale else {
        unreachable!();
    };
    stale_command.review_set_sha256 = "0".repeat(64);
    assert!(stale_store.execute(stale).is_err());

    let foreign_store = seeded_store();
    let mut foreign = approval("foreign-task-breakdown", 1);
    let DeliveryCommand::ApproveTaskBreakdown(foreign_command) = &mut foreign else {
        unreachable!();
    };
    foreign_command.delivery_id = DeliveryId("foreign-delivery".into());
    assert!(foreign_store.execute(foreign).is_err());

    let revised_store = seeded_store();
    let revised = revised_store
        .execute(approval("revised-task-breakdown", 0))
        .expect_err("revised expectedRevision must be rejected");
    assert_eq!(revised.code(), DeliveryStoreErrorCode::RevisionConflict);

    let mut changed = approved_delivery().into_snapshot();
    changed.attention_items[0].context = changed.attention_items[0]
        .context
        .replace(REVIEW_SET_SHA256, &"1".repeat(64));
    let changed = Delivery::try_from_snapshot(changed).expect("opaque changed review");
    let changed_journal = Arc::new(InMemoryDeliveryJournal::new());
    seed_journal(changed_journal.as_ref(), &changed, "seed-changed-review");
    let changed_store = DeliveryStore::new(changed_journal);
    assert!(
        changed_store
            .execute(approval("changed-review-task-breakdown", 1))
            .is_err()
    );
}

#[test]
fn task_breakdown_store_replay_returns_the_original_graph() {
    let store = seeded_store();
    let first = store
        .execute(approval("replay-task-breakdown", 1))
        .expect("first DeliveryCommand::ApproveTaskBreakdown");
    let replay = store
        .execute(approval("replay-task-breakdown", 1))
        .expect("same task approval replays");

    assert!(!first.replayed);
    assert!(replay.replayed);
    assert_eq!(
        replay.snapshot.snapshot().tasks,
        first.snapshot.snapshot().tasks
    );
    assert_eq!(replay.task_breakdown_event, first.task_breakdown_event);
}

#[test]
fn generic_append_cannot_write_task_breakdown_approved() {
    let store = seeded_store();
    let source = approved_delivery();
    let rejected = store
        .execute(DeliveryCommand::Append(AppendDelivery {
            delivery_id: source.id().clone(),
            request_id: RequestId("generic-task-breakdown".into()),
            request_digest: "d".repeat(64),
            operation: DeliveryMutationOperation::TaskBreakdownApproved,
            expected_revision: source.revision(),
            snapshot: source,
        }))
        .expect_err("generic append is rejected");
    assert_eq!(rejected.code(), DeliveryStoreErrorCode::InvalidStoreOptions);
}

#[test]
fn task_breakdown_revision_race_has_one_winner_and_no_partial_graph() {
    let store = seeded_store();
    let winner = store
        .execute(approval("task-breakdown-race-winner", 1))
        .expect("first DeliveryCommand::ApproveTaskBreakdown wins");
    let loser = store
        .execute(approval("task-breakdown-race-loser", 1))
        .expect_err("stale concurrent task promotion loses");

    assert_eq!(loser.code(), DeliveryStoreErrorCode::RevisionConflict);
    let current = store
        .query(DeliveryQuery::Get(DeliveryId("delivery-main".into())))
        .expect("current Delivery");
    assert_eq!(current.snapshot().tasks, winner.snapshot.snapshot().tasks);
    assert_eq!(current.snapshot().tasks.len(), 1);
}
