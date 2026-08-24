use std::sync::Arc;

use winwincode_delivery::{
    domain::Delivery,
    store::{
        AppendDelivery, CreateDelivery, DeliveryCommand, DeliveryCommandPort, DeliveryQuery,
        DeliveryQueryPort, DeliveryStore, DeliveryStoreErrorCode, InMemoryDeliveryJournal,
    },
};
use winwincode_domain::RequestId;

const REQUEST_A: &str = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
const REQUEST_B: &str = "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb";

fn snapshot(revision: u64, status: &str) -> Delivery {
    let template = include_str!("fixtures/delivery-store.json");
    let text = template
        .replace("\"revision\": 1", &format!("\"revision\": {revision}"))
        .replace(
            "\"status\": \"draft\"",
            &format!("\"status\": \"{status}\""),
        )
        .replace(
            "\"updatedAtMillis\": 1800000000001",
            &format!("\"updatedAtMillis\": {}", 1_800_000_000_000_u64 + revision),
        );
    Delivery::decode_json(text.as_bytes()).expect("valid store fixture")
}

#[test]
fn append_only_store_replays_idempotent_request() {
    let backend = Arc::new(InMemoryDeliveryJournal::new());
    let store = DeliveryStore::new(Arc::clone(&backend));
    store
        .execute(DeliveryCommand::Create(CreateDelivery {
            request_id: RequestId("create-delivery".into()),
            request_digest: REQUEST_A.into(),
            snapshot: snapshot(1, "draft"),
        }))
        .expect("create");
    let append = AppendDelivery {
        delivery_id: snapshot(1, "draft").id().clone(),
        request_id: RequestId("update-spec".into()),
        request_digest: REQUEST_B.into(),
        operation: "delivery.spec.updated".parse().expect("operation"),
        expected_revision: 1,
        snapshot: snapshot(2, "ready"),
    };

    let first = store
        .execute(DeliveryCommand::Append(append.clone()))
        .expect("first append");
    let replay = store
        .execute(DeliveryCommand::Append(append))
        .expect("replay");

    assert!(!first.replayed);
    assert!(replay.replayed);
    assert_eq!(first.snapshot, replay.snapshot);
}

#[test]
fn append_only_store_rejects_revision_race() {
    let backend = Arc::new(InMemoryDeliveryJournal::new());
    let store = DeliveryStore::new(Arc::clone(&backend));
    store
        .execute(DeliveryCommand::Create(CreateDelivery {
            request_id: RequestId("create-delivery".into()),
            request_digest: REQUEST_A.into(),
            snapshot: snapshot(1, "draft"),
        }))
        .expect("create");
    store
        .execute(DeliveryCommand::Append(AppendDelivery {
            delivery_id: snapshot(1, "draft").id().clone(),
            request_id: RequestId("left-update".into()),
            request_digest: REQUEST_A.into(),
            operation: "delivery.spec.updated".parse().expect("operation"),
            expected_revision: 1,
            snapshot: snapshot(2, "ready"),
        }))
        .expect("winner");

    let error = store
        .execute(DeliveryCommand::Append(AppendDelivery {
            delivery_id: snapshot(1, "draft").id().clone(),
            request_id: RequestId("right-update".into()),
            request_digest: REQUEST_B.into(),
            operation: "delivery.spec.updated".parse().expect("operation"),
            expected_revision: 1,
            snapshot: snapshot(2, "ready"),
        }))
        .expect_err("stale expected revision must lose");
    assert_eq!(error.code(), DeliveryStoreErrorCode::RevisionConflict);

    let stored = store
        .query(DeliveryQuery::Get(snapshot(1, "draft").id().clone()))
        .expect("read");
    assert_eq!(stored.records.len(), 2);
}
