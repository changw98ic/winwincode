use std::sync::Arc;

use serde::Serialize;
use sha2::{Digest, Sha256};
use winwincode_delivery::{
    domain::Delivery,
    store::{
        AppendDelivery, AtomicPublication, DELIVERY_STORE_SCHEMA_VERSION, DeliveryCommand,
        DeliveryCommandPort, DeliveryJournalCodec, DeliveryJournalPort, DeliveryMutationOperation,
        DeliveryQuery, DeliveryQueryPort, DeliveryStore, DeliveryStoreErrorCode,
        DeliveryStoreManifest, DeliveryStoreRecord, InMemoryDeliveryJournal, JournalEntryState,
        JournalRecordBytes,
    },
};
use winwincode_domain::{DeliveryId, RequestId};

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
fn generic_append_cannot_hide_a_task_graph_under_another_operation() {
    let mut snapshot = approved_delivery().into_snapshot();
    snapshot.revision += 1;
    let fixture: serde_json::Value =
        serde_json::from_str(include_str!("fixtures/delivery-main.json")).unwrap();
    snapshot.tasks = serde_json::from_value(fixture["tasks"].clone()).unwrap();
    assert!(
        !snapshot.tasks.is_empty(),
        "fixture supplies a real forged graph"
    );
    let promoted = Delivery::try_from_snapshot(snapshot).expect("structurally valid graph");

    for operation in [
        DeliveryMutationOperation::SessionBound,
        DeliveryMutationOperation::DeliverySpecUpdated,
    ] {
        let store = seeded_store();
        let rejected = store
            .execute(DeliveryCommand::Append(AppendDelivery {
                delivery_id: promoted.id().clone(),
                request_id: RequestId(format!("hidden-task-graph-{operation:?}")),
                request_digest: "e".repeat(64),
                operation,
                expected_revision: 1,
                snapshot: promoted.clone(),
            }))
            .expect_err("another generic operation cannot smuggle the task graph");
        assert_eq!(rejected.code(), DeliveryStoreErrorCode::InvalidStoreOptions);
        let current = store
            .query(DeliveryQuery::Get(DeliveryId(
                "dlv_01J00000000000000000000000".into(),
            )))
            .expect("current Delivery");
        assert!(current.snapshot().tasks.is_empty());
        assert_eq!(current.revision(), 1);
    }
}
