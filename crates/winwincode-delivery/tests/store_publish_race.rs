// SPDX-License-Identifier: Apache-2.0
use std::sync::{Arc, Barrier};
use winwincode_delivery::{
    domain::Delivery,
    store::{
        AtomicPublication, CreateDelivery, CreateDeliveryWorkItems, DeliveryCommand,
        DeliveryCommandPort, DeliveryJournalPort, DeliveryStore, InMemoryDeliveryJournal,
        JournalBackendError, LoadedDeliveryJournal,
    },
};
use winwincode_domain::{DeliveryId, RequestId, Revision, WorkItem, WorkItemId, WorkItemState};

struct RacingJournal {
    inner: InMemoryDeliveryJournal,
    barrier: Barrier,
}
impl DeliveryJournalPort for RacingJournal {
    fn load(&self, id: &DeliveryId) -> Result<Option<LoadedDeliveryJournal>, JournalBackendError> {
        self.inner.load(id)
    }
    fn publish(&self, publication: AtomicPublication) -> Result<(), JournalBackendError> {
        if matches!(publication, AtomicPublication::Append { .. }) {
            self.barrier.wait();
        }
        self.inner.publish(publication)
    }
}

#[test]
fn concurrent_same_request_replays_only_the_identical_snapshot() {
    for changed in [false, true] {
        let mut value: serde_json::Value =
            serde_json::from_slice(include_bytes!("fixtures/delivery-store.json")).unwrap();
        value["updatedAtMillis"] = value["createdAtMillis"].clone();
        let delivery = Delivery::decode_json(&serde_json::to_vec(&value).unwrap())
            .expect("canonical empty Delivery");
        let contract = &delivery.snapshot().work_run_aggregate.contract;
        let item = WorkItem {
            schema_version: contract.schema_version.clone(),
            id: WorkItemId("wit_01J00000000000000000000002".into()),
            work_contract_id: contract.id.clone(),
            work_contract_revision: contract.revision.clone(),
            revision: Revision(1),
            state: WorkItemState::Ready,
            title: "First title".into(),
            goal: "Persist the exact requested task".into(),
            criterion_ids: contract
                .criteria
                .iter()
                .map(|criterion| criterion.id.clone())
                .collect(),
            depends_on: Vec::new(),
        };
        let backend = Arc::new(RacingJournal {
            inner: InMemoryDeliveryJournal::new(),
            barrier: Barrier::new(2),
        });
        DeliveryStore::new(backend.clone())
            .execute(DeliveryCommand::Create(CreateDelivery {
                request_id: RequestId("create-race-delivery".into()),
                request_digest: "a".repeat(64),
                snapshot: delivery.clone(),
            }))
            .expect("seed");
        let command = CreateDeliveryWorkItems {
            delivery_id: delivery.id().clone(),
            request_id: RequestId("same-racing-request".into()),
            request_digest: "b".repeat(64),
            expected_revision: delivery.revision(),
            contract_revision: 1,
            items: vec![item],
            now_millis: delivery.snapshot().updated_at_millis + 1,
        };
        let mut other = command.clone();
        if changed {
            other.items[0].title = "Conflicting title".into();
        }
        let results = std::thread::scope(|scope| {
            let left_backend = backend.clone();
            let right_backend = backend.clone();
            let left = scope.spawn(move || {
                DeliveryStore::new(left_backend)
                    .execute(DeliveryCommand::CreateWorkItems(Box::new(command)))
            });
            let right = scope.spawn(move || {
                DeliveryStore::new(right_backend)
                    .execute(DeliveryCommand::CreateWorkItems(Box::new(other)))
            });
            [
                left.join().expect("left thread"),
                right.join().expect("right thread"),
            ]
        });
        assert_eq!(
            results.iter().filter(|result| result.is_ok()).count(),
            if changed { 1 } else { 2 },
            "changed={changed}: {results:?}"
        );
        assert_eq!(
            results
                .iter()
                .filter_map(|result| result.as_ref().ok())
                .filter(|result| result.replayed)
                .count(),
            usize::from(!changed)
        );
        assert_eq!(
            backend.load(delivery.id()).unwrap().unwrap().records.len(),
            2
        );
    }
}

#[test]
fn create_cannot_seed_work_items_or_execution_runs_without_their_commands() {
    let source: serde_json::Value = serde_json::from_slice(include_bytes!(
        "../../../tests/fixtures/contracts/execution-port.valid.json"
    ))
    .unwrap();
    let job = source["messages"]
        .as_array()
        .unwrap()
        .iter()
        .find(|message| message["kind"] == "job.dispatch")
        .unwrap()["job"]
        .clone();
    for include_run in [true, false] {
        let mut value: serde_json::Value =
            serde_json::from_slice(include_bytes!("fixtures/delivery-store.json")).unwrap();
        value["updatedAtMillis"] = value["createdAtMillis"].clone();
        value["workRunAggregate"]["contract"] = job["workInput"]["workContract"].clone();
        value["workRunAggregate"]["items"] =
            serde_json::json!([job["workInput"]["workItem"].clone()]);
        value["workRunAggregate"]["items"][0]["state"] = "ready".into();
        if include_run {
            let scope = &job["scope"];
            value["workRunAggregate"]["items"][0]["state"] = "in_progress".into();
            value["workRunAggregate"]["runs"] = serde_json::json!([{
                "schemaVersion":"winwincode/v1", "id":scope["workRunId"],
                "workContractId":scope["workContractId"], "contractRevision":scope["workContractRevision"],
                "workItemId":scope["workItemId"], "workItemRevision":scope["workItemRevision"],
                "revision":1, "state":"leased", "executionJobId":job["jobId"], "attempt":1,
                "workerId":"wrk_01J00000000000000000000000", "workerInstanceId":"wki_01J00000000000000000000000",
                "workerSessionId":"wsn_01J00000000000000000000000", "leaseId":"lse_01J00000000000000000000000",
                "fencingToken":"1", "productSessionId":scope["productSessionId"], "codexThreadId":null, "candidateDigest":null
            }]);
        }
        let delivery = Delivery::decode_json(&serde_json::to_vec(&value).unwrap())
            .expect("valid shape does not grant write authority");
        let journal = Arc::new(InMemoryDeliveryJournal::new());
        let store = DeliveryStore::new(Arc::clone(&journal));
        let result = store.execute(DeliveryCommand::Create(CreateDelivery {
            request_id: RequestId("create-must-not-seed-runtime".into()),
            request_digest: "9".repeat(64),
            snapshot: delivery.clone(),
        }));
        assert!(
            result.is_err(),
            "Create must reject prepopulated work facts, include_run={include_run}"
        );
        assert!(journal.load(delivery.id()).unwrap().is_none());
    }
}
