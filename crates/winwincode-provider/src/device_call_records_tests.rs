// SPDX-License-Identifier: Apache-2.0

use super::*;
use std::path::PathBuf;

fn setup(case: &str) -> (PathBuf, DeviceProviderStore, ModelOpenMessage) {
    let root = std::env::temp_dir().join(format!(
        "wwc-call-pairs-{case}-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    let store = DeviceProviderStore::open(&root).unwrap();
    let fixture: serde_json::Value = serde_json::from_str(include_str!(
        "../../../tests/fixtures/contracts/execution-port.valid.json"
    ))
    .unwrap();
    let open = serde_json::from_value(
        fixture["messages"]
            .as_array()
            .unwrap()
            .iter()
            .find(|message| message["kind"] == "model.open")
            .unwrap()
            .clone(),
    )
    .unwrap();
    (root, store, open)
}

#[test]
fn terminal_response_and_original_request_commit_together_or_remain_pending() {
    let (root, store, open) = setup("atomic");
    store.connection.execute_batch("CREATE TRIGGER fail_pair BEFORE INSERT ON model_response_exchange BEGIN SELECT RAISE(ABORT,'injected pair failure'); END;").unwrap();
    assert!(
        store
            .execute_model_with(&open, &|| true, || Ok(vec![model_failure(
                &open,
                "DEVICE_PROVIDER_UNAVAILABLE"
            )]))
            .is_err()
    );
    let (request, status, chunks, accounting): (String, String, Option<String>, Option<String>) =
        store
            .connection
            .query_row(
                "SELECT request_open,call_status,chunks,accounting_chunks FROM exchanges",
                [],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)),
            )
            .unwrap();
    assert_eq!(request, serde_json::to_string(&open).unwrap());
    assert_eq!(status, "pending");
    assert!(chunks.is_none());
    assert!(accounting.is_none());
    store
        .connection
        .execute_batch("DROP TRIGGER fail_pair")
        .unwrap();
    assert_eq!(
        store
            .fail_terminated_worker_calls(&open.lease.worker_instance_id.0)
            .unwrap(),
        1
    );
    let result = store.replay_model(&open.model_exchange_id.0, 1).unwrap();
    assert_eq!(
        result[0].error.as_ref().unwrap().message,
        "DEVICE_MODEL_INTERRUPTED"
    );
    assert_eq!(
        store
            .fail_terminated_worker_calls(&open.lease.worker_instance_id.0)
            .unwrap(),
        0
    );
    assert!(
        store
            .retain_stored_model_exchange_chunks(&open, &[model_failure(&open, "late success")])
            .is_err()
    );
    drop(store);
    std::fs::remove_dir_all(root).unwrap();
}

#[test]
fn recovery_only_closes_matching_terminated_owner_and_keeps_physical_request_and_unknown_cost() {
    let (root, store, open) = setup("owner");
    let request = serde_json::to_string(&open).unwrap();
    store
        .connection
        .execute(
            "INSERT INTO exchanges(exchange_id,digest,request_open) VALUES(?1,?2,?3)",
            params![
                open.model_exchange_id.0,
                format!("{:x}", Sha256::digest(request.as_bytes())),
                request
            ],
        )
        .unwrap();
    let payload = b"actual prepared model input";
    store.connection.execute("INSERT INTO model_invocation_attempts(exchange_id,attempt_number,adapter_request_id,state,request_json,request_payload) VALUES(?1,1,'old-attempt','invoking',?2,?3)",params![open.model_exchange_id.0,request,payload.as_slice()]).unwrap();
    assert_eq!(
        store
            .fail_terminated_worker_calls("other-instance")
            .unwrap(),
        0
    );
    let guard = store
        .try_model_exchange_owner(&open.model_exchange_id.0)
        .unwrap()
        .unwrap();
    assert!(
        store
            .fail_terminated_worker_calls(&open.lease.worker_instance_id.0)
            .is_err()
    );
    drop(guard);
    assert_eq!(
        store
            .fail_terminated_worker_calls(&open.lease.worker_instance_id.0)
            .unwrap(),
        1
    );
    let (input,state,status,result,accounting):(Vec<u8>,String,String,String,Option<String>)=store.connection.query_row("SELECT request_payload,state,call_status,failure_chunks,accounting_chunks FROM model_invocation_attempts",[],|row|Ok((row.get(0)?,row.get(1)?,row.get(2)?,row.get(3)?,row.get(4)?))).unwrap();
    assert_eq!(input, payload);
    assert_eq!(state, "interrupted_unknown");
    assert_eq!(status, "failed");
    assert!(result.contains("DEVICE_MODEL_INTERRUPTED"));
    assert!(accounting.is_none(), "unknown usage stays unknown");
    drop(store);
    std::fs::remove_dir_all(root).unwrap();
}

#[test]
fn cancellation_retains_request_and_cancelled_result_even_when_cancel_arrives_first() {
    let (root, store, open) = setup("cancel");
    store.cancel_model(&open.model_exchange_id.0).unwrap();
    assert!(
        store
            .execute_model_authorized(&open, || true)
            .unwrap()
            .is_empty()
    );
    let (request, status, result): (String, String, String) = store
        .connection
        .query_row(
            "SELECT request_open,call_status,chunks FROM exchanges",
            [],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
        )
        .unwrap();
    assert_eq!(request, serde_json::to_string(&open).unwrap());
    assert_eq!(status, "cancelled");
    assert!(result.contains("DEVICE_MODEL_CANCELLED"));
    assert!(
        store
            .replay_model(&open.model_exchange_id.0, 1)
            .unwrap()
            .is_empty()
    );
    drop(store);
    std::fs::remove_dir_all(root).unwrap();
}

#[test]
fn interrupted_jev_calls_keep_measured_usage_and_close_pending_physical_results() {
    let (root, store, open) = setup("jev-cost");
    let request = serde_json::to_string(&open).unwrap();
    store
        .connection
        .execute(
            "INSERT INTO exchanges(exchange_id,digest,request_open) VALUES(?1,?2,?3)",
            params![
                open.model_exchange_id.0,
                format!("{:x}", Sha256::digest(request.as_bytes())),
                request
            ],
        )
        .unwrap();
    let operation = format!("jev:{}:1", open.model_exchange_id.0);
    store.connection.execute("INSERT INTO jev_context_exchanges(operation_id,digest,request_json) VALUES(?1,'digest','original scoring request')", [&operation]).unwrap();
    let invocation = serde_json::json!({"providerId":"jev-provider","modelId":"jev-model","request":{"input":"actual input"}}).to_string();
    let success = serde_json::json!({"value":{"score":0.8},"observation":{"providerId":"jev-provider","modelId":"jev-model","latency":{"secs":0,"nanos":0},"inputTokens":40,"outputTokens":8,"resolvedModelId":null,"batchSize":1,"device":"remote","confidence":0.8}}).to_string();
    store.connection.execute("INSERT INTO jev_invocation_attempts(operation_id,sequence,request_json,status,result_json) VALUES(?1,1,?2,'succeeded',?3)", params![operation,invocation,success]).unwrap();
    store.connection.execute("INSERT INTO jev_invocation_attempts(operation_id,sequence,request_json) VALUES(?1,2,?2)", params![operation,invocation]).unwrap();
    store
        .fail_terminated_worker_calls(&open.lease.worker_instance_id.0)
        .unwrap();
    let (status, result): (String, String) = store
        .connection
        .query_row(
            "SELECT call_status,result FROM jev_context_exchanges WHERE operation_id=?1",
            [&operation],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .unwrap();
    assert_eq!(status, "failed");
    let result: serde_json::Value = serde_json::from_str(&result).unwrap();
    assert_eq!(result["observation"]["inputTokens"], 40);
    assert_eq!(result["observation"]["outputTokens"], 8);
    let typed: crate::JevRun<serde_json::Value> = serde_json::from_value(result.clone()).unwrap();
    assert_eq!(typed.observation.unwrap().input_tokens, 40);
    assert!(
        result["failures"]
            .as_array()
            .unwrap()
            .iter()
            .any(|failure| failure["network"]["acceptance"] == "unknown")
    );
    let physical: Vec<(String, String)> = store
        .connection
        .prepare("SELECT status,result_json FROM jev_invocation_attempts ORDER BY sequence")
        .unwrap()
        .query_map([], |row| Ok((row.get(0)?, row.get(1)?)))
        .unwrap()
        .collect::<Result<_, _>>()
        .unwrap();
    assert_eq!(physical[0], ("succeeded".into(), success));
    assert_eq!(physical[1].0, "failed");
    assert!(physical[1].1.contains("interrupted"));
    drop(store);
    std::fs::remove_dir_all(root).unwrap();
}
