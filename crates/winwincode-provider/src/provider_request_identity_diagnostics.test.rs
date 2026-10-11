// SPDX-License-Identifier: Apache-2.0

const IDENTITY_SESSION_A: &str = "00000000-0000-0000-0000-000000000301";
const IDENTITY_SESSION_B: &str = "00000000-0000-0000-0000-000000000302";
const IDENTITY_THREAD_A: &str = "00000000-0000-0000-0000-000000000101";
const IDENTITY_THREAD_B: &str = "00000000-0000-0000-0000-000000000102";

fn identity_payload(thread_id: &str) -> Vec<u8> {
    let session_id = if thread_id == IDENTITY_THREAD_B {
        IDENTITY_SESSION_B
    } else {
        IDENTITY_SESSION_A
    };
    serde_json::to_vec(&serde_json::json!({
        "requestId":"identity-fixture-request", "provider":"provider-https-fixture",
        "sessionId":session_id, "threadId":thread_id,
        "turnId":"00000000-0000-0000-0000-000000000201",
        "request":{"model":"fixture-model", "instructions":"Reply briefly",
            "input":[{"type":"message","role":"user","content":[{"type":"input_text","text":"public fixture"}]}],
            "stream":true, "store":false, "tool_choice":"none", "parallel_tool_calls":false,
            "client_metadata":{"thread_id":thread_id,"session_id":session_id},
            "reasoning":{"effort":"max"}}
    })).unwrap()
}

fn identity_settings(fixture: &TlsFixture, protocol: u8) -> HttpsSseProviderConfig {
    match protocol {
        0 => config(fixture),
        1 => config(fixture)
            .with_openai_chat_completions(8192, ProviderTokenPricing::default())
            .unwrap(),
        2 => config(fixture)
            .with_anthropic_messages(8192, ProviderTokenPricing::default())
            .unwrap(),
        3 => config(fixture).with_openai_responses_text(8192).unwrap(),
        _ => unreachable!(),
    }
}

fn identity_http_attempt(
    adapter: &HttpsSseProviderAdapter,
    ordinal: u8,
    payload: &[u8],
) -> winwincode_network::NetworkFailure {
    let exchange = ModelExchangeId(format!("mdl_{ordinal:026}"));
    let request_id = RequestId(format!("req_{ordinal:026}"));
    let adapter_id = format!("pad_{ordinal:026}");
    let mut request = invocation(&exchange, &request_id);
    request.adapter_request_id = &adapter_id;
    request.payload = payload;
    adapter
        .open_https(request, SECRET)
        .unwrap_err()
        .network_failure()
}

fn identity_header_values(request: &[u8], name: &str) -> Vec<String> {
    let end = find_bytes(request, b"\r\n\r\n").unwrap();
    std::str::from_utf8(&request[..end])
        .unwrap()
        .lines()
        .filter_map(|line| {
            let (key, value) = line.split_once(':')?;
            key.eq_ignore_ascii_case(name)
                .then(|| value.trim().to_owned())
        })
        .collect()
}

fn identity_error_response(status: &'static str) -> TestResponse<'static> {
    TestResponse {
        status,
        content_type: "application/json",
        body: "{\"error\":{\"type\":\"public_fixture\"}}",
        declared_length: None,
        delay: Duration::ZERO,
    }
}

fn concurrent_identity_wire(protocol: u8) {
    let fixture = TlsFixture::start(vec![
        identity_error_response("400 Bad Request"),
        identity_error_response("400 Bad Request"),
    ]);
    let adapter = HttpsSseProviderAdapter::try_new(identity_settings(&fixture, protocol)).unwrap();
    let a = identity_payload(IDENTITY_THREAD_A);
    let b = identity_payload(IDENTITY_THREAD_B);
    let barrier = std::sync::Barrier::new(2);
    thread::scope(|scope| {
        let first = scope.spawn(|| {
            barrier.wait();
            identity_http_attempt(&adapter, 1, &a)
        });
        let second = scope.spawn(|| {
            barrier.wait();
            identity_http_attempt(&adapter, 2, &b)
        });
        assert_eq!(first.join().unwrap().http_status, Some(400));
        assert_eq!(second.join().unwrap().http_status, Some(400));
    });
    let requests = fixture.finish();
    assert_eq!(requests.len(), 2);
    let mut sessions: Vec<_> = requests
        .iter()
        .flat_map(|request| identity_header_values(request, "session-id"))
        .collect();
    sessions.sort();
    assert_eq!(sessions, vec![IDENTITY_SESSION_A, IDENTITY_SESSION_B]);
    let mut observed = Vec::new();
    for request in requests {
        let values = identity_header_values(&request, "thread-id");
        assert_eq!(
            values.len(),
            1,
            "native conversation identity header missing or duplicated"
        );
        let expected_session = match values[0].as_str() {
            IDENTITY_THREAD_A => IDENTITY_SESSION_A,
            IDENTITY_THREAD_B => IDENTITY_SESSION_B,
            _ => panic!("unexpected native conversation identity"),
        };
        assert_eq!(
            identity_header_values(&request, "session-id"),
            vec![expected_session],
            "native session and thread must stay paired on each physical request"
        );
        observed.push(values[0].clone());
        assert_eq!(identity_header_values(&request, "session-id").len(), 1);
        let agent = identity_header_values(&request, "user-agent");
        assert_eq!(agent.len(), 1);
        assert!(
            agent[0].contains("WinWinCode"),
            "native client must identify itself honestly"
        );
        assert!(
            identity_header_values(&request, "x-opencode-session").is_empty(),
            "ordinary origin must not receive vendor alias"
        );
    }
    observed.sort();
    assert_eq!(observed, vec![IDENTITY_THREAD_A, IDENTITY_THREAD_B]);
}

#[test]
fn identity_native_canonical_concurrent_conversations() {
    concurrent_identity_wire(0);
}
#[test]
fn identity_native_chat_completions_concurrent_conversations() {
    concurrent_identity_wire(1);
}
#[test]
fn identity_native_anthropic_concurrent_conversations() {
    concurrent_identity_wire(2);
}
#[test]
fn identity_native_responses_concurrent_conversations() {
    concurrent_identity_wire(3);
}

#[test]
fn identity_native_retry_keeps_the_original_conversation_and_idempotency_key() {
    let fixture = TlsFixture::start(vec![
        identity_error_response("503 Service Unavailable"),
        identity_error_response("400 Bad Request"),
    ]);
    let adapter = HttpsSseProviderAdapter::try_new(identity_settings(&fixture, 1)).unwrap();
    let payload = identity_payload(IDENTITY_THREAD_A);
    assert!(identity_http_attempt(&adapter, 1, &payload).retryable());
    assert!(!identity_http_attempt(&adapter, 1, &payload).retryable());
    let requests = fixture.finish();
    assert_eq!(requests.len(), 2);
    for request in requests {
        assert_eq!(
            identity_header_values(&request, "thread-id"),
            vec![IDENTITY_THREAD_A]
        );
        assert_eq!(
            identity_header_values(&request, "session-id"),
            vec![IDENTITY_SESSION_A]
        );
        assert_eq!(
            identity_header_values(&request, "idempotency-key"),
            vec!["pad_00000000000000000000000001"]
        );
    }
}

#[test]
fn identity_explicit_custom_headers_override_once() {
    let fixture = TlsFixture::start(vec![identity_error_response("400 Bad Request")]);
    let settings = identity_settings(&fixture, 1)
        .with_custom_headers([
            ("Session-ID".into(), "explicit-conversation".into()),
            ("User-Agent".into(), "Public Fixture Client".into()),
            (
                "x-opencode-session".into(),
                "explicit-vendor-conversation".into(),
            ),
        ])
        .unwrap();
    let adapter = HttpsSseProviderAdapter::try_new(settings).unwrap();
    assert_eq!(
        identity_http_attempt(&adapter, 1, &identity_payload(IDENTITY_THREAD_A)).http_status,
        Some(400)
    );
    let requests = fixture.finish();
    assert_eq!(requests.len(), 1);
    assert_eq!(
        identity_header_values(&requests[0], "session-id"),
        vec!["explicit-conversation"]
    );
    assert_eq!(
        identity_header_values(&requests[0], "thread-id"),
        vec![IDENTITY_THREAD_A]
    );
    assert_eq!(
        identity_header_values(&requests[0], "user-agent"),
        vec!["Public Fixture Client"]
    );
    assert_eq!(
        identity_header_values(&requests[0], "x-opencode-session"),
        vec!["explicit-vendor-conversation"]
    );
}

#[test]
fn identity_raw_canonical_preserves_existing_transport_behavior() {
    let fixture = TlsFixture::start(vec![identity_error_response("400 Bad Request")]);
    let adapter = HttpsSseProviderAdapter::try_new(config(&fixture)).unwrap();
    assert_eq!(
        identity_http_attempt(&adapter, 1, PAYLOAD).http_status,
        Some(400)
    );
    let requests = fixture.finish();
    assert_eq!(requests.len(), 1);
    assert!(identity_header_values(&requests[0], "session-id").is_empty());
    assert!(identity_header_values(&requests[0], "thread-id").is_empty());
    assert!(identity_header_values(&requests[0], "x-opencode-session").is_empty());
}

#[test]
fn identity_raw_canonical_identity_named_fields_are_not_a_core_envelope() {
    let fixture = TlsFixture::start(vec![identity_error_response("400 Bad Request")]);
    let adapter = HttpsSseProviderAdapter::try_new(config(&fixture)).unwrap();
    let value = serde_json::json!({"sessionId":"public raw session", "threadId":"public raw thread\r\n", "prompt":"public fixture"});
    let payload = serde_json::to_vec(&value).unwrap();
    assert_eq!(
        identity_http_attempt(&adapter, 1, &payload).http_status,
        Some(400)
    );
    let requests = fixture.finish();
    assert_eq!(requests.len(), 1);
    assert!(identity_header_values(&requests[0], "session-id").is_empty());
    assert!(identity_header_values(&requests[0], "thread-id").is_empty());
    assert!(identity_header_values(&requests[0], "x-opencode-session").is_empty());
    let start = find_bytes(&requests[0], b"\r\n\r\n").unwrap() + 4;
    assert_eq!(
        serde_json::from_slice::<serde_json::Value>(&requests[0][start..]).unwrap(),
        value
    );
}

#[test]
fn identity_native_complete_envelope_without_both_identities_is_rejected() {
    let mut value: serde_json::Value =
        serde_json::from_slice(&identity_payload(IDENTITY_THREAD_A)).unwrap();
    value.as_object_mut().unwrap().remove("sessionId");
    value.as_object_mut().unwrap().remove("threadId");
    let payload = serde_json::to_vec(&value).unwrap();
    assert_eq!(
        native_request_headers(&payload, "https://opencode.ai/v1/messages", &[])
            .unwrap_err()
            .kind(),
        crate::ProviderAdapterErrorKind::RequestTranslation
    );
    for protocol in 0..4 {
        let fixture = TlsFixture::start(Vec::new());
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        listener.set_nonblocking(true).unwrap();
        let mut settings = identity_settings(&fixture, protocol);
        settings.endpoint = format!(
            "https://localhost:{}/v1/model",
            listener.local_addr().unwrap().port()
        );
        let adapter = HttpsSseProviderAdapter::try_new(settings).unwrap();
        let exchange = ModelExchangeId("mdl_00000000000000000000000009".into());
        let request_id = RequestId("req_00000000000000000000000009".into());
        let mut request = invocation(&exchange, &request_id);
        request.payload = &payload;
        assert_eq!(
            adapter.open_https(request, SECRET).unwrap_err().kind(),
            crate::ProviderAdapterErrorKind::RequestTranslation
        );
        assert!(adapter.shared.streams.lock().unwrap().is_empty());
        assert!(
            matches!(listener.accept(),Err(error) if error.kind()==std::io::ErrorKind::WouldBlock)
        );
        assert!(fixture.finish().is_empty());
    }
}

#[test]
fn identity_vendor_alias_is_restricted_to_the_exact_official_https_origin() {
    let payload = identity_payload(IDENTITY_THREAD_A);
    for endpoint in [
        "https://opencode.ai/v1/messages",
        "https://opencode.ai/inference/go/openai/v1/chat/completions",
        "https://OPENCODE.AI:443/v1/messages",
    ] {
        let headers = native_request_headers(&payload, endpoint, &[]).unwrap();
        let alias: Vec<_> = headers
            .iter()
            .filter(|(name, _)| *name == "x-opencode-session")
            .map(|(_, value)| value.as_str())
            .collect();
        assert_eq!(alias, vec![IDENTITY_THREAD_A]);
    }
    for endpoint in [
        "http://opencode.ai/v1/messages",
        "https://opencode.ai:444/v1/messages",
        "https://opencode.ai.evil.invalid/v1/messages",
        "https://evil-opencode.ai/v1/messages",
        "https://evil.invalid/opencode.ai",
        "https://opencode.ai@evil.invalid/v1/messages",
        "https://evil.invalid@opencode.ai/v1/messages",
    ] {
        let headers = native_request_headers(&payload, endpoint, &[]).unwrap();
        assert!(
            !headers
                .iter()
                .any(|(name, _)| *name == "x-opencode-session"),
            "vendor identity leaked to another origin"
        );
    }
}

#[test]
fn identity_vendor_alias_custom_override_is_case_insensitive() {
    let headers = native_request_headers(
        &identity_payload(IDENTITY_THREAD_A),
        "https://opencode.ai/v1/messages",
        &[
            (
                "X-OpenCode-Session".into(),
                "explicit-vendor-conversation".into(),
            ),
            ("SESSION-ID".into(), "explicit-session".into()),
            ("THREAD-ID".into(), "explicit-thread".into()),
            ("USER-AGENT".into(), "Public Fixture Client".into()),
        ],
    )
    .unwrap();
    assert!(
        headers.is_empty(),
        "explicit headers must not be accompanied by duplicate defaults"
    );
}

#[test]
fn identity_native_control_characters_are_rejected_before_tcp_or_stream_state() {
    for protocol in 0..4 {
        for field in ["sessionId", "threadId"] {
            let fixture = TlsFixture::start(Vec::new());
            let listener = TcpListener::bind("127.0.0.1:0").unwrap();
            listener.set_nonblocking(true).unwrap();
            let mut settings = identity_settings(&fixture, protocol);
            settings.endpoint = format!(
                "https://localhost:{}/v1/model",
                listener.local_addr().unwrap().port()
            );
            let adapter = HttpsSseProviderAdapter::try_new(settings).unwrap();
            let mut value: serde_json::Value =
                serde_json::from_slice(&identity_payload(IDENTITY_THREAD_A)).unwrap();
            value[field] = "invalid\r\nInjected-Header: public-fixture".into();
            let payload = serde_json::to_vec(&value).unwrap();
            let exchange = ModelExchangeId("mdl_00000000000000000000000009".into());
            let request_id = RequestId("req_00000000000000000000000009".into());
            let mut request = invocation(&exchange, &request_id);
            request.payload = &payload;
            assert_eq!(
                adapter.open_https(request, SECRET).unwrap_err().kind(),
                crate::ProviderAdapterErrorKind::RequestTranslation
            );
            assert!(adapter.shared.streams.lock().unwrap().is_empty());
            assert!(
                matches!(listener.accept(),Err(error) if error.kind()==std::io::ErrorKind::WouldBlock),
                "invalid identity opened a socket"
            );
            assert!(fixture.finish().is_empty());
        }
    }
}

#[test]
fn identity_partial_or_invalid_envelopes_never_fall_back_to_client_metadata() {
    for field in ["sessionId", "threadId"] {
        let original: serde_json::Value =
            serde_json::from_slice(&identity_payload(IDENTITY_THREAD_A)).unwrap();
        let mut missing = original.clone();
        missing.as_object_mut().unwrap().remove(field);
        assert_eq!(
            native_request_headers(
                &serde_json::to_vec(&missing).unwrap(),
                "https://opencode.ai/v1/messages",
                &[]
            )
            .unwrap_err()
            .kind(),
            crate::ProviderAdapterErrorKind::RequestTranslation
        );
        for invalid in [
            serde_json::Value::Null,
            serde_json::json!(123),
            serde_json::json!(""),
            serde_json::json!("has space"),
            serde_json::json!("non-ascii-身份"),
            serde_json::json!("x".repeat(513)),
        ] {
            let mut value = original.clone();
            value[field] = invalid;
            assert_eq!(
                native_request_headers(
                    &serde_json::to_vec(&value).unwrap(),
                    "https://opencode.ai/v1/messages",
                    &[]
                )
                .unwrap_err()
                .kind(),
                crate::ProviderAdapterErrorKind::RequestTranslation
            );
        }
    }
}
