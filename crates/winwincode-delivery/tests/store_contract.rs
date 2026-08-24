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
    snapshot_for("delivery-store-main", revision, status)
}

fn snapshot_for(delivery_id: &str, revision: u64, status: &str) -> Delivery {
    let template = include_str!("fixtures/delivery-store.json");
    let text = template
        .replace("delivery-store-main", delivery_id)
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
fn create_replays_the_same_request_and_rejects_conflicting_reuse() {
    let backend = Arc::new(InMemoryDeliveryJournal::new());
    let store = DeliveryStore::new(Arc::clone(&backend));
    let command = CreateDelivery {
        request_id: RequestId("create-delivery".into()),
        request_digest: REQUEST_A.into(),
        snapshot: snapshot(1, "draft"),
    };

    let first = store
        .execute(DeliveryCommand::Create(command.clone()))
        .expect("first create");
    let replay = store
        .execute(DeliveryCommand::Create(command.clone()))
        .expect("create replay");
    assert!(!first.replayed);
    assert!(replay.replayed);
    assert_eq!(first.snapshot, replay.snapshot);

    let mut conflicting = command;
    conflicting.request_digest = REQUEST_B.into();
    let error = store
        .execute(DeliveryCommand::Create(conflicting))
        .expect_err("conflicting request reuse");
    assert_eq!(error.code(), DeliveryStoreErrorCode::RequestConflict);
}

#[test]
fn delivery_journal_scopes_request_replay_to_one_delivery() {
    let backend = Arc::new(InMemoryDeliveryJournal::new());
    let store = DeliveryStore::new(Arc::clone(&backend));

    for delivery_id in ["delivery-store-left", "delivery-store-right"] {
        let result = store
            .execute(DeliveryCommand::Create(CreateDelivery {
                request_id: RequestId("create-delivery".into()),
                request_digest: REQUEST_A.into(),
                snapshot: snapshot_for(delivery_id, 1, "draft"),
            }))
            .expect("request identity is scoped by the Control Plane before this journal seam");
        assert!(!result.replayed);
        assert_eq!(result.snapshot.id().0, delivery_id);
    }
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
    assert_eq!(error.expected_revision(), Some(1));
    assert_eq!(error.current_revision(), Some(2));

    let delivery: Delivery = store
        .query(DeliveryQuery::Get(snapshot(1, "draft").id().clone()))
        .expect("read");
    assert_eq!(delivery.revision(), 2);
}

#[test]
fn query_rejects_invalid_delivery_identity_before_storage() {
    let store = DeliveryStore::new(Arc::new(InMemoryDeliveryJournal::new()));
    let error = store
        .query(DeliveryQuery::Get(winwincode_domain::DeliveryId(
            "delivery\ninvalid".into(),
        )))
        .expect_err("invalid Delivery identity");
    assert_eq!(error.code(), DeliveryStoreErrorCode::InvalidStoreOptions);
}
