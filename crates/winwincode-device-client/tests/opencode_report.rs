// SPDX-License-Identifier: Apache-2.0

//! Exercise usage numbers through the native durable report boundary.

use winwincode_api::generated::DeviceProviderReport;
use winwincode_client_port::exchange::{FrameCodec, FrameOutbox};
use winwincode_client_port::messages::{ClientToServerEnvelope, ClientToServerMessage};
use winwincode_device_client::DeviceStore;

#[test]
fn usage_requires_a_finite_numeric_percentage() {
    for percent in ["null", "\"12.125\"", "{}", "1e999"] {
        let json = format!(
            r#"{{"percent":{percent},"resetsAt":"2026-10-06T08:19:51.000Z","status":"ok"}}"#
        );
        assert!(
            serde_json::from_str::<winwincode_api::generated::DeviceProviderOpenCodeUsageWindow>(
                &json
            )
            .is_err()
        );
    }
}

#[test]
fn fractional_usage_survives_native_report_storage_and_restart() {
    let root = std::env::temp_dir().join(format!("opencode-report-{}", std::process::id()));
    let report: DeviceProviderReport = serde_json::from_value(serde_json::json!({
        "receipt": null,
        "snapshot": {
            "clientNodeId": "cnd_00000000000000000000000001", "revision": 8,
            "encryptionPublicKey": "fixture", "providers": [],
            "openCodeAccounts": [{
                "accountRef": "oca_00000000000000000000000001",
                "subject": "fixture-user", "email": "fixture@example.test",
                "issuer": "https://opencode.ai/console",
                "state": "authorized", "credentialVersion": 1,
                "usage": {
                    "organizationId": "fixture-org", "updatedAtMs": 1_791_260_080_829_i64,
                    "rolling": {"percent": 0.0, "resetsAt": "2026-10-06T08:19:51.000Z", "status": "ok"},
                    "weekly": {"percent": 12.125, "resetsAt": "2026-10-12T00:00:00.000Z", "status": "ok"},
                    "monthly": {"percent": 100.0, "resetsAt": "2026-11-06T02:08:13.000Z", "status": "ok"}
                }
            }]
        }
    })).unwrap();
    let envelope = ClientToServerEnvelope {
        schema_version: "winwincode/v1".into(),
        message_id: "fixture-report".into(),
        client_node_id: report.snapshot.client_node_id.clone(),
        client_instance_id: "fixture-instance".into(),
        sequence: 1,
        occurred_at: "2026-10-06T04:14:40.829Z".into(),
        message: ClientToServerMessage::ProviderReport(Box::new(report)),
    };
    let codec = FrameCodec::default();
    let frame = codec.encode_envelope(&envelope).unwrap();
    let mut store = DeviceStore::open(&root).unwrap();
    store
        .bind_outbox_stream(&envelope.client_node_id, &envelope.client_instance_id)
        .unwrap();
    store
        .append(0, &frame)
        .expect("usage report must enter the native outbox");
    drop(store);
    let mut store = DeviceStore::open(&root).unwrap();
    store
        .bind_outbox_stream(&envelope.client_node_id, &envelope.client_instance_id)
        .unwrap();
    let restored = store.load().unwrap().unwrap();
    assert_eq!(restored.frames, [frame]);
    let decoded: ClientToServerEnvelope = codec.decode(&restored.frames[0].frame).unwrap();
    assert_eq!(decoded, envelope);
    drop(store);
    std::fs::remove_dir_all(root).unwrap();
}
