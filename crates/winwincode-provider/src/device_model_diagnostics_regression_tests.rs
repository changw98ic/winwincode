// SPDX-License-Identifier: Apache-2.0

use super::*;

#[test]
fn maximum_diagnostic_combination_keeps_bounded_valid_wire_json_and_sse_reference() {
    let fixture: serde_json::Value = serde_json::from_str(include_str!(
        "../../../tests/fixtures/contracts/execution-port.valid.json"
    ))
    .unwrap();
    let open: ModelOpenMessage = serde_json::from_value(
        fixture["messages"]
            .as_array()
            .unwrap()
            .iter()
            .find(|message| message["kind"] == "model.open")
            .unwrap()
            .clone(),
    )
    .unwrap();
    let log = format!("sse-{}.log", "a".repeat(64));
    for status in ["retained", "write_failed"] {
        let original: winwincode_network::NetworkFailure =
            serde_json::from_value(serde_json::json!({
                "kind": "transport_interrupted", "acceptance": "response_received",
                "phase": "response_headers", "httpStatus": 599, "retryAfterMs": u64::MAX,
                "diagnostic": {
                    "code": "response_headers_too_large", "field": "usage_output_tokens",
                    "ioKind": "connection_aborted", "osCode": i32::MIN,
                    "line": u32::MAX, "column": u32::MAX,
                    "responseLog": log, "responseLogStatus": status
                }
            }))
            .unwrap();
        let mut chunk = model_failure(&open, "DEVICE_PROVIDER_SSE_EVENT_INVALID");
        chunk.error.as_mut().unwrap().message = "界".repeat(500);
        append_network_failure(&mut chunk, original);
        let message = &chunk.error.as_ref().unwrap().message;
        assert!(message.chars().count() <= 500);
        let (_, retained_json) = message.split_once(";network=").unwrap();
        let retained: winwincode_network::NetworkFailure =
            serde_json::from_str(retained_json).unwrap();
        assert_eq!(retained.kind, original.kind);
        assert_eq!(retained.http_status, original.http_status);
        let diagnostic = serde_json::to_value(retained.diagnostic.unwrap()).unwrap();
        assert_eq!(diagnostic["responseLog"], log);
        assert_eq!(diagnostic["responseLogStatus"], status);
        assert_eq!(diagnostic["field"], "usage_output_tokens");
        assert_eq!(original.diagnostic.unwrap().line, Some(u32::MAX));
    }
}
