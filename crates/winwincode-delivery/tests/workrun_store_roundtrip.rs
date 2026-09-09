// SPDX-License-Identifier: Apache-2.0

use std::{fs, sync::Arc};
use winwincode_delivery::{
    domain::Delivery,
    store::{
        CreateDelivery, DeliveryCommand, DeliveryCommandPort, DeliveryJournalCodec,
        DeliveryJournalPort, DeliveryQuery, DeliveryQueryPort, DeliveryStore,
        InMemoryDeliveryJournal,
    },
};
use winwincode_domain::RequestId;

#[test]
fn workrun_aggregate_survives_persistent_record_reopen() {
    let mut value: serde_json::Value =
        serde_json::from_str(include_str!("fixtures/delivery-main.json")).unwrap();
    for name in [
        "sessionBindings",
        "stageRuns",
        "attentionItems",
        "evidence",
        "tasks",
    ] {
        value[name] = serde_json::json!([]);
    }
    value["verdict"] = serde_json::Value::Null;
    value["status"] = serde_json::json!("draft");
    value["revision"] = serde_json::json!(1);
    value["workRunAggregate"]["items"][0]["state"] = "in_progress".into();
    let item = &value["workRunAggregate"]["items"][0];
    value["workRunAggregate"]["runs"] = serde_json::json!([{
        "schemaVersion":"winwincode/v1", "id":"wrn_01J00000000000000000000000",
        "workContractId":item["workContractId"], "contractRevision":item["workContractRevision"],
        "workItemId":item["id"], "workItemRevision":item["revision"],
        "revision":1, "state":"leased", "executionJobId":"job_01J00000000000000000000000", "attempt":1,
        "workerId":"wrk_01J00000000000000000000000", "workerInstanceId":"wki_01J00000000000000000000000",
        "workerSessionId":"wsn_01J00000000000000000000000", "leaseId":"lse_01J00000000000000000000000",
        "fencingToken":"1", "productSessionId":null, "codexThreadId":null, "candidateDigest":null
    }]);
    let delivery = Delivery::decode_json(&serde_json::to_vec(&value).unwrap()).unwrap();
    assert_eq!(delivery.snapshot().work_run_aggregate.items.len(), 1);
    assert_eq!(delivery.snapshot().work_run_aggregate.runs.len(), 1);
    let backend = Arc::new(InMemoryDeliveryJournal::new());
    let store = DeliveryStore::new(Arc::clone(&backend));
    store
        .execute(DeliveryCommand::SeedForTest(CreateDelivery {
            request_id: RequestId("persist-create".into()),
            request_digest: "a".repeat(64),
            snapshot: delivery.clone(),
        }))
        .unwrap();
    let loaded = store
        .query(DeliveryQuery::Get(delivery.id().clone()))
        .unwrap();
    let record = backend
        .load(delivery.id())
        .unwrap()
        .unwrap()
        .records
        .into_iter()
        .next()
        .unwrap();
    let bytes = record.bytes.clone();
    let path = std::env::temp_dir().join(format!("wwc-delivery-reopen-{}", std::process::id()));
    fs::write(&path, bytes).unwrap();
    drop(store);
    let reopened = DeliveryJournalCodec::decode_record(&fs::read(&path).unwrap()).unwrap();
    fs::remove_file(path).unwrap();
    assert_eq!(
        loaded.snapshot().work_run_aggregate,
        reopened.snapshot.snapshot().work_run_aggregate
    );
}
