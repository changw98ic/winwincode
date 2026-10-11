// SPDX-License-Identifier: Apache-2.0
use super::DeviceModels;
use base64::{Engine as _, engine::general_purpose::STANDARD};
use sha2::{Digest as _, Sha256};
use winwincode_execution_port::generated::{EncodedPayload, ModelOpenMessage};
use winwincode_provider::{DeviceProviderStore, audit_model_replay_metrics, model_failure};

#[test]
#[ignore = "mechanism audit: measured empty recovery cost over 1/10 MiB synthetic history"]
fn mechanism_actual_empty_worker_model_queue_reloads_foreign_history() {
    let fixture: serde_json::Value = serde_json::from_str(include_str!(
        "../../../tests/fixtures/contracts/execution-port.valid.json"
    ))
    .unwrap();
    let template: ModelOpenMessage = serde_json::from_value(
        fixture["messages"]
            .as_array()
            .unwrap()
            .iter()
            .find(|m| m["kind"] == "model.open")
            .unwrap()
            .clone(),
    )
    .unwrap();
    let mut cases = Vec::new();
    for raw_history_bytes in [1024 * 1024_usize, 10 * 1024 * 1024] {
        let root = tempfile::tempdir().unwrap();
        let directory = root.path().join("device");
        let store = DeviceProviderStore::open(&directory).unwrap();
        let mut models = DeviceModels::open(&directory).unwrap();
        let mut stored_bytes = 0_u64;
        for index in 0..3 {
            let mut open = template.clone();
            open.model_exchange_id.0 = format!("mdl_{index:026}");
            if index != 0 {
                open.worker_session_id.0 = format!("wsn_{:026}", 1000 + index);
                open.session_identity.worker_session_id = open.worker_session_id.clone();
                open.lease.worker_instance_id.0 = format!("wki_{:026}", 1000 + index);
                assert_ne!(open.worker_session_id, template.worker_session_id);
                assert_ne!(
                    open.lease.worker_instance_id,
                    template.lease.worker_instance_id
                );
            } else {
                models.note_open_identity(&open);
            }
            let mut chunk = model_failure(&open, "SYNTHETIC_OFFLINE_HISTORY");
            chunk.error = None;
            let payload = serde_json::json!({"type":"completed", "syntheticPadding":"x".repeat(raw_history_bytes/3)}).to_string();
            chunk.payload = Some(EncodedPayload {
                content_type: "application/json".into(),
                data_base64: STANDARD.encode(&payload),
                payload_digest: winwincode_domain::Sha256Digest(format!(
                    "sha256:{:x}",
                    Sha256::digest(payload.as_bytes())
                )),
            });
            stored_bytes += serde_json::to_vec(&vec![chunk.clone()]).unwrap().len() as u64;
            store
                .retain_stored_model_exchange_chunks(&open.model_exchange_id.0, &[chunk])
                .unwrap();
        }
        assert!(models.next_chunk().unwrap().unwrap().is_final);
        assert_eq!(models.delivered_high_water.len(), 1);
        audit_model_replay_metrics::reset();
        let started = std::time::Instant::now();
        for _ in 0..100 {
            assert!(models.next_chunk().unwrap().is_none());
        }
        let elapsed_ms = started.elapsed().as_secs_f64() * 1000.0;
        let metrics = audit_model_replay_metrics::take();
        assert_eq!(
            metrics.select_chunks_rows, 300,
            "every actual empty turn rereads one owned plus two foreign responses"
        );
        assert_eq!(metrics.chunks_json_bytes_deserialized, stored_bytes * 100);
        assert_eq!(
            models.logged_foreign.len(),
            2,
            "log deduplication does not suppress actual reads"
        );
        assert!(
            models.pending.is_empty(),
            "no physical Provider thread is admitted"
        );
        cases.push(serde_json::json!({"raw_history_bytes":raw_history_bytes,"stored_history_json_bytes":stored_bytes,
            "empty_next_chunk_calls":100,"elapsed_ms":elapsed_ms,"metrics":metrics,
            "owned_exchanges":1,"foreign_exchanges":2,"physical_provider_requests":0}));
    }
    let value = serde_json::json!({"schema_version":1,"source_commit":"e994faa55ac2baad964bea5c43a23d919497a9de",
        "method":"DeviceModels::next_chunk -> recover_from_durable_store -> DeviceProviderStore::replay_model", "cases":cases});
    println!("MODEL_MECHANISM_AUDIT {value}");
    if let Some(path) = std::env::var_os("WWC_MECHANISM_AUDIT_OUTPUT") {
        let path = std::path::Path::new(&path);
        std::fs::create_dir_all(path).unwrap();
        std::fs::write(
            path.join("actual-empty-worker-model-queue.json"),
            serde_json::to_vec_pretty(&value).unwrap(),
        )
        .unwrap();
    }
}
