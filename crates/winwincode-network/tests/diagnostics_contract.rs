// SPDX-License-Identifier: Apache-2.0

use serde_json::json;
use winwincode_network::{Acceptance, ErrorKind, NetworkFailure, Phase};

fn legacy_failure() -> serde_json::Value {
    json!({
        "kind": "connection_unavailable",
        "acceptance": "not_sent",
        "phase": "connect",
        "httpStatus": null,
        "retryAfterMs": null
    })
}

#[test]
fn old_network_failure_json_remains_readable_and_omits_absent_diagnostic() {
    let old = legacy_failure();
    let failure: NetworkFailure = serde_json::from_value(old.clone()).unwrap();
    assert_eq!(serde_json::to_value(failure).unwrap(), old);
    assert_eq!(
        serde_json::to_value(NetworkFailure::new(
            ErrorKind::ConnectionUnavailable,
            Acceptance::NotSent,
            Phase::Connect,
        ))
        .unwrap(),
        old
    );
}

#[test]
fn safe_io_diagnostic_survives_json_round_trip() {
    let mut encoded = legacy_failure();
    encoded["diagnostic"] = json!({
        "code": "io",
        "ioKind": "connection_refused",
        "osCode": 61
    });
    let failure: NetworkFailure = serde_json::from_value(encoded.clone()).unwrap();
    assert_eq!(serde_json::to_value(failure).unwrap(), encoded);
}

#[test]
fn diagnostic_rejects_arbitrary_codes_and_payload_fields() {
    for diagnostic in [
        json!({"code": "SYNTHETIC_PRIVATE_TEXT"}),
        json!({"code": "io", "message": "SYNTHETIC_PRIVATE_TEXT"}),
        json!({"code": "io", "url": "https://invalid.example/?token=SYNTHETIC_CREDENTIAL"}),
        json!({"code": "io", "ioKind": "SYNTHETIC_PRIVATE_TEXT"}),
    ] {
        let mut encoded = legacy_failure();
        encoded["diagnostic"] = diagnostic;
        assert!(serde_json::from_value::<NetworkFailure>(encoded).is_err());
    }
}

#[test]
fn each_http_error_retains_its_status_and_safe_diagnostic() {
    for (status, kind) in [
        (400, ErrorKind::RequestInvalid),
        (401, ErrorKind::Authentication),
        (403, ErrorKind::Authorization),
        (408, ErrorKind::Timeout),
        (413, ErrorKind::RequestInvalid),
        (429, ErrorKind::RateLimited),
        (503, ErrorKind::ServerTransient),
    ] {
        let failure = NetworkFailure::http(status, None);
        assert_eq!(failure.kind, kind);
        assert_eq!(failure.http_status, Some(status));
        assert_eq!(
            serde_json::to_value(failure).unwrap()["diagnostic"],
            json!({"code": "http_status"})
        );
    }
}

#[test]
fn sse_response_reference_survives_without_accepting_a_path_or_payload() {
    let mut encoded = legacy_failure();
    let log = format!("sse-{}.log", "a".repeat(64));
    encoded["diagnostic"] = json!({
        "code": "sse_event", "responseLog": log, "responseLogStatus": "retained"
    });
    let restored: NetworkFailure = serde_json::from_value(encoded.clone()).unwrap();
    assert_eq!(serde_json::to_value(restored).unwrap(), encoded);
    for bad in [
        "../../private.log",
        "SYNTHETIC_PRIVATE_RESPONSE",
        "sse-short.log",
    ] {
        encoded["diagnostic"]["responseLog"] = json!(bad);
        assert!(serde_json::from_value::<NetworkFailure>(encoded.clone()).is_err());
    }
    encoded["diagnostic"]["responseLog"] = json!(log);
    encoded["diagnostic"]["responseLogStatus"] = json!("SYNTHETIC_PRIVATE_ERROR");
    assert!(serde_json::from_value::<NetworkFailure>(encoded).is_err());
}
