// SPDX-License-Identifier: Apache-2.0

use std::sync::Arc;
use winwincode_delivery::{
    domain::Delivery,
    store::{
        CreateDelivery, CreateDeliveryWorkItems, DeliveryCommand, DeliveryCommandPort,
        DeliveryStore, DeliveryStoreErrorCode, InMemoryDeliveryJournal,
    },
};
use winwincode_domain::RequestId;

fn fixtures() -> (Delivery, winwincode_domain::WorkItem) {
    let mut value: serde_json::Value =
        serde_json::from_str(include_str!("fixtures/delivery-main.json")).unwrap();
    let mut item_value = value["workRunAggregate"]["items"][0].clone();
    item_value["revision"] = serde_json::json!(1);
    item_value["state"] = serde_json::json!("ready");
    let item = serde_json::from_value(item_value).unwrap();
    value["workRunAggregate"]["items"] = serde_json::json!([]);
    value["workRunAggregate"]["runs"] = serde_json::json!([]);
    value["sessionBindings"] = serde_json::json!([]);
    value["stageRuns"] = serde_json::json!([]);
    value["attentionItems"] = serde_json::json!([]);
    value["evidence"] = serde_json::json!([]);
    value["verdict"] = serde_json::Value::Null;
    value["tasks"] = serde_json::json!([]);
    value["status"] = serde_json::json!("draft");
    value["revision"] = serde_json::json!(1);
    let delivery = Delivery::decode_json(&serde_json::to_vec(&value).unwrap()).unwrap();
    (delivery, item)
}

#[test]
fn create_work_items_persists_replays_and_rejects_generic_forgery() {
    let (delivery, item) = fixtures();
    let backend = Arc::new(InMemoryDeliveryJournal::new());
    let store = DeliveryStore::new(backend);
    store
        .execute(DeliveryCommand::SeedForTest(CreateDelivery {
            request_id: RequestId("create-work-items-delivery".into()),
            request_digest: "a".repeat(64),
            snapshot: delivery.clone(),
        }))
        .unwrap();
    let command = CreateDeliveryWorkItems {
        delivery_id: delivery.id().clone(),
        request_id: RequestId("create-work-items".into()),
        request_digest: "b".repeat(64),
        expected_revision: 1,
        contract_revision: 1,
        items: vec![item.clone()],
        now_millis: delivery.snapshot().updated_at_millis + 1,
    };
    let first = store
        .execute(DeliveryCommand::CreateWorkItems(Box::new(command.clone())))
        .unwrap();
    assert_eq!(first.snapshot.revision(), 2);
    assert_eq!(
        first.snapshot.snapshot().work_run_aggregate.items,
        vec![item.clone()]
    );
    assert!(
        store
            .execute(DeliveryCommand::CreateWorkItems(Box::new(command)))
            .unwrap()
            .replayed
    );
    let forged = store
        .execute(DeliveryCommand::Append(
            winwincode_delivery::store::AppendDelivery {
                delivery_id: delivery.id().clone(),
                request_id: RequestId("forge-work-items".into()),
                request_digest: "c".repeat(64),
                operation: "workitems.created".parse().unwrap(),
                expected_revision: 2,
                snapshot: first.snapshot.clone(),
            },
        ))
        .unwrap_err();
    assert_eq!(forged.code(), DeliveryStoreErrorCode::RevisionConflict);
}

#[test]
fn replay_rejects_changed_items_and_contract_revision() {
    let (delivery, item) = fixtures();
    let backend = Arc::new(InMemoryDeliveryJournal::new());
    let store = DeliveryStore::new(backend);
    store
        .execute(DeliveryCommand::SeedForTest(CreateDelivery {
            request_id: RequestId("create-work-items-replay".into()),
            request_digest: "d".repeat(64),
            snapshot: delivery.clone(),
        }))
        .unwrap();
    let command = CreateDeliveryWorkItems {
        delivery_id: delivery.id().clone(),
        request_id: RequestId("work-items-replay".into()),
        request_digest: "e".repeat(64),
        expected_revision: 1,
        contract_revision: 1,
        items: vec![item.clone()],
        now_millis: delivery.snapshot().updated_at_millis + 1,
    };
    store
        .execute(DeliveryCommand::CreateWorkItems(Box::new(command.clone())))
        .unwrap();
    let mut changed = item;
    changed.title.push_str(" changed");
    let conflict = store
        .execute(DeliveryCommand::CreateWorkItems(Box::new(
            CreateDeliveryWorkItems {
                items: vec![changed],
                ..command
            },
        )))
        .unwrap_err();
    assert_eq!(conflict.code(), DeliveryStoreErrorCode::RequestConflict);
}

#[test]
fn replay_rejects_contract_revision_and_item_subset_changes() {
    let (delivery, item) = fixtures();
    let backend = Arc::new(InMemoryDeliveryJournal::new());
    let store = DeliveryStore::new(backend);
    store
        .execute(DeliveryCommand::SeedForTest(CreateDelivery {
            request_id: RequestId("create-work-items-shape".into()),
            request_digest: "f".repeat(64),
            snapshot: delivery.clone(),
        }))
        .unwrap();
    let command = CreateDeliveryWorkItems {
        delivery_id: delivery.id().clone(),
        request_id: RequestId("work-items-shape".into()),
        request_digest: "1".repeat(64),
        expected_revision: 1,
        contract_revision: 1,
        items: vec![item.clone()],
        now_millis: delivery.snapshot().updated_at_millis + 1,
    };
    store
        .execute(DeliveryCommand::CreateWorkItems(Box::new(command.clone())))
        .unwrap();
    let changed_revision = store
        .execute(DeliveryCommand::CreateWorkItems(Box::new(
            CreateDeliveryWorkItems {
                contract_revision: 2,
                ..command.clone()
            },
        )))
        .unwrap_err();
    assert_eq!(
        changed_revision.code(),
        DeliveryStoreErrorCode::RequestConflict
    );
    let subset = store
        .execute(DeliveryCommand::CreateWorkItems(Box::new(
            CreateDeliveryWorkItems {
                items: vec![],
                ..command
            },
        )))
        .unwrap_err();
    assert_eq!(subset.code(), DeliveryStoreErrorCode::RequestConflict);
}

#[test]
fn create_work_items_rejects_duplicate_and_foreign_contract_items() {
    let (delivery, item) = fixtures();
    let backend = Arc::new(InMemoryDeliveryJournal::new());
    let store = DeliveryStore::new(backend);
    store
        .execute(DeliveryCommand::SeedForTest(CreateDelivery {
            request_id: RequestId("create-work-items-invalid".into()),
            request_digest: "2".repeat(64),
            snapshot: delivery.clone(),
        }))
        .unwrap();
    let base = CreateDeliveryWorkItems {
        delivery_id: delivery.id().clone(),
        request_id: RequestId("invalid-items".into()),
        request_digest: "3".repeat(64),
        expected_revision: 1,
        contract_revision: 1,
        items: vec![item.clone(), item.clone()],
        now_millis: delivery.snapshot().updated_at_millis + 1,
    };
    assert_eq!(
        store
            .execute(DeliveryCommand::CreateWorkItems(Box::new(base)))
            .unwrap_err()
            .code(),
        DeliveryStoreErrorCode::InvalidStoreOptions
    );
    let mut foreign = item;
    foreign.work_contract_id.0 = "wct_01J00000000000000000000099".into();
    let command = CreateDeliveryWorkItems {
        delivery_id: delivery.id().clone(),
        request_id: RequestId("foreign-item".into()),
        request_digest: "4".repeat(64),
        expected_revision: 1,
        contract_revision: 1,
        items: vec![foreign],
        now_millis: delivery.snapshot().updated_at_millis + 1,
    };
    assert_eq!(
        store
            .execute(DeliveryCommand::CreateWorkItems(Box::new(command)))
            .unwrap_err()
            .code(),
        DeliveryStoreErrorCode::InvalidStoreOptions
    );
}

#[test]
fn create_work_items_rejects_dependency_cycle() {
    let (delivery, mut item) = fixtures();
    item.depends_on = vec![item.id.clone()];
    let store = DeliveryStore::new(Arc::new(InMemoryDeliveryJournal::new()));
    store
        .execute(DeliveryCommand::SeedForTest(CreateDelivery {
            request_id: RequestId("create-work-items-cycle".into()),
            request_digest: "5".repeat(64),
            snapshot: delivery.clone(),
        }))
        .unwrap();
    let command = CreateDeliveryWorkItems {
        delivery_id: delivery.id().clone(),
        request_id: RequestId("cycle-items".into()),
        request_digest: "6".repeat(64),
        expected_revision: 1,
        contract_revision: 1,
        items: vec![item],
        now_millis: delivery.snapshot().updated_at_millis + 1,
    };
    assert_eq!(
        store
            .execute(DeliveryCommand::CreateWorkItems(Box::new(command)))
            .unwrap_err()
            .code(),
        DeliveryStoreErrorCode::InvalidStoreOptions
    );
}
