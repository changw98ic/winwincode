// SPDX-License-Identifier: Apache-2.0

const PRIVATE_HTTP_ERROR: &str = r#"{"error":{"code":"invalid_request","message":"private provider diagnostic https://private.invalid/token provider-https-sse-secret-fixture","param":"reasoning_effort"}}"#;

struct HttpErrorLogDirectory(std::path::PathBuf);

impl HttpErrorLogDirectory {
    fn new() -> Self {
        use std::os::unix::fs::DirBuilderExt as _;
        static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        loop {
            let path = std::env::temp_dir().join(format!(
                "wwc-http-error-body-{}-{}-{}",
                std::process::id(),
                std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .unwrap()
                    .as_nanos(),
                NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
            ));
            match std::fs::DirBuilder::new().mode(0o700).create(&path) {
                Ok(()) => return Self(path),
                Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {}
                Err(error) => panic!("private test directory: {:?}", error.kind()),
            }
        }
    }
}

impl Drop for HttpErrorLogDirectory {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn retained_http_error(
    directory: &HttpErrorLogDirectory,
    failure: winwincode_network::NetworkFailure,
) -> (serde_json::Value, Vec<u8>) {
    use std::os::unix::fs::PermissionsExt as _;
    let safe = serde_json::to_string(&failure).unwrap();
    assert!(!safe.contains("private provider diagnostic"));
    assert!(!safe.contains("https://private.invalid"));
    assert!(!safe.contains(std::str::from_utf8(SECRET).unwrap()));
    let diagnostic = failure.diagnostic.expect("safe HTTP diagnostic");
    assert_eq!(
        diagnostic.response_log_status,
        Some(winwincode_network::ResponseLogStatus::Retained),
        "non-2xx response body was discarded before a durable private reference"
    );
    let reference = diagnostic.response_log.expect("private body log reference");
    let path = directory.0.join(reference.to_string());
    assert_eq!(
        std::fs::metadata(&directory.0)
            .unwrap()
            .permissions()
            .mode()
            & 0o777,
        0o700
    );
    assert_eq!(
        std::fs::metadata(&path).unwrap().permissions().mode() & 0o777,
        0o600
    );
    let bytes = std::fs::read(path).unwrap();
    let separator = bytes.iter().position(|byte| *byte == b'\n').unwrap();
    (
        serde_json::from_slice(&bytes[..separator]).unwrap(),
        bytes[separator + 1..].to_vec(),
    )
}

fn fixture_http_failure(
    fixture: &TlsFixture,
    directory: &HttpErrorLogDirectory,
    settings: HttpsSseProviderConfig,
) -> winwincode_network::NetworkFailure {
    let adapter = HttpsSseProviderAdapter::try_new(settings)
        .unwrap()
        .with_sse_failure_log(directory.0.clone());
    let exchange = ModelExchangeId("mdl_00000000000000000000000001".into());
    let request_id = RequestId("req_00000000000000000000000001".into());
    assert_eq!(adapter.shared.config.endpoint, fixture.endpoint);
    adapter
        .open_https(invocation(&exchange, &request_id), SECRET)
        .unwrap_err()
        .network_failure()
}

#[test]
fn non_success_json_body_keeps_private_reference_and_original_http_failure() {
    let fixture = TlsFixture::start(vec![TestResponse {
        status: "400 Bad Request",
        content_type: "application/json",
        body: PRIVATE_HTTP_ERROR,
        declared_length: None,
        delay: Duration::ZERO,
    }]);
    let directory = HttpErrorLogDirectory::new();
    let failure = fixture_http_failure(&fixture, &directory, config(&fixture));
    assert_eq!(
        fixture.finish().len(),
        1,
        "permanent HTTP 400 must not retry"
    );
    assert_eq!(failure.http_status, Some(400));
    assert_eq!(failure.kind, winwincode_network::ErrorKind::RequestInvalid);
    assert!(!failure.retryable());
    let (metadata, body) = retained_http_error(&directory, failure);
    assert_eq!(body, PRIVATE_HTTP_ERROR.as_bytes());
    assert_eq!(metadata["capture"]["complete"], true);
    assert_eq!(metadata["capture"]["truncated"], false);
    assert!(metadata["capture"]["readFailure"].is_null());
}

#[test]
fn all_http_status_classes_keep_exact_body_and_original_retry_policy() {
    for (status, number) in [
        ("401 Unauthorized", 401),
        ("413 Content Too Large", 413),
        ("429 Too Many Requests", 429),
        ("503 Service Unavailable", 503),
    ] {
        let fixture = TlsFixture::start(vec![TestResponse {
            status,
            content_type: "application/json",
            body: PRIVATE_HTTP_ERROR,
            declared_length: None,
            delay: Duration::ZERO,
        }]);
        let directory = HttpErrorLogDirectory::new();
        let failure = fixture_http_failure(&fixture, &directory, config(&fixture));
        assert_eq!(
            fixture.finish().len(),
            1,
            "one physical invocation, {status}"
        );
        let expected = winwincode_network::NetworkFailure::http(number, None);
        assert_eq!(failure.http_status, expected.http_status);
        assert_eq!(failure.kind, expected.kind);
        assert_eq!(failure.phase, expected.phase);
        assert_eq!(failure.acceptance, expected.acceptance);
        assert_eq!(failure.retryable(), expected.retryable());
        assert_eq!(
            retained_http_error(&directory, failure).1,
            PRIVATE_HTTP_ERROR.as_bytes()
        );
    }
}

#[test]
fn http_error_body_limit_retains_only_prefix_without_reclassifying_400() {
    let body = "private provider diagnostic ".repeat(100);
    let fixture = TlsFixture::start(vec![TestResponse {
        status: "400 Bad Request",
        content_type: "text/plain",
        body: &body,
        declared_length: None,
        delay: Duration::ZERO,
    }]);
    let directory = HttpErrorLogDirectory::new();
    let mut settings = config(&fixture);
    settings.max_response_bytes = 128;
    settings.max_event_bytes = 128;
    let failure = fixture_http_failure(&fixture, &directory, settings);
    assert_eq!(fixture.finish().len(), 1);
    assert_eq!(failure.http_status, Some(400));
    assert!(!failure.retryable());
    let (metadata, bytes) = retained_http_error(&directory, failure);
    assert_eq!(bytes, body.as_bytes()[..128]);
    assert_eq!(metadata["capture"]["limitBytes"], 128);
    assert_eq!(metadata["capture"]["truncated"], true);
    assert_eq!(metadata["capture"]["complete"], false);
}

#[test]
fn large_config_does_not_expand_the_private_http_error_hard_limit() {
    let body = "private provider diagnostic ".repeat(5000);
    let fixture = TlsFixture::start(vec![TestResponse {
        status: "400 Bad Request",
        content_type: "text/plain",
        body: &body,
        declared_length: None,
        delay: Duration::ZERO,
    }]);
    let directory = HttpErrorLogDirectory::new();
    let mut settings = config(&fixture);
    settings.max_response_bytes = 128 * 1024;
    let failure = fixture_http_failure(&fixture, &directory, settings);
    assert_eq!(fixture.finish().len(), 1);
    assert_eq!(failure.http_status, Some(400));
    assert!(!failure.retryable());
    let (metadata, bytes) = retained_http_error(&directory, failure);
    assert_eq!(bytes, body.as_bytes()[..64 * 1024]);
    assert_eq!(metadata["capture"]["limitBytes"], 64 * 1024);
    assert_eq!(metadata["capture"]["truncated"], true);
    assert_eq!(metadata["capture"]["complete"], false);
}

#[test]
fn slow_http_error_body_obeys_original_total_deadline_and_keeps_400() {
    let body = "private provider diagnostic ".repeat(100);
    let fixture = TlsFixture::start_streaming(
        vec![TestResponse {
            status: "400 Bad Request",
            content_type: "text/plain",
            body: &body,
            declared_length: None,
            delay: Duration::ZERO,
        }],
        Duration::from_millis(80),
    );
    let directory = HttpErrorLogDirectory::new();
    let mut settings = config(&fixture);
    settings.connect_timeout = Duration::from_millis(250);
    settings.total_timeout = Duration::from_millis(250);
    settings.idle_timeout = Duration::from_millis(250);
    let started = std::time::Instant::now();
    let failure = fixture_http_failure(&fixture, &directory, settings);
    let elapsed = started.elapsed();
    assert!(
        elapsed < Duration::from_millis(450),
        "original 250ms deadline was replaced: {elapsed:?}"
    );
    assert_eq!(fixture.finish().len(), 1);
    assert_eq!(failure.http_status, Some(400));
    assert!(!failure.retryable());
    let (metadata, bytes) = retained_http_error(&directory, failure);
    assert!(bytes.len() < body.len());
    assert_eq!(metadata["capture"]["complete"], false);
    assert_eq!(metadata["capture"]["truncated"], false);
    assert!(metadata["capture"]["readFailure"].is_object());
}

#[test]
fn interrupted_http_error_body_preserves_received_status_and_partial_bytes() {
    let fixture = TlsFixture::start(vec![TestResponse {
        status: "400 Bad Request",
        content_type: "text/plain",
        body: PRIVATE_HTTP_ERROR,
        declared_length: Some(PRIVATE_HTTP_ERROR.len() + 100),
        delay: Duration::ZERO,
    }]);
    let directory = HttpErrorLogDirectory::new();
    let failure = fixture_http_failure(&fixture, &directory, config(&fixture));
    assert_eq!(fixture.finish().len(), 1);
    assert_eq!(failure.http_status, Some(400));
    assert!(!failure.retryable());
    let (metadata, bytes) = retained_http_error(&directory, failure);
    assert_eq!(bytes, PRIVATE_HTTP_ERROR.as_bytes());
    assert_eq!(metadata["capture"]["complete"], false);
    assert!(metadata["capture"]["readFailure"].is_object());
}

#[test]
fn http_error_capture_stays_finite_when_successful_stream_deadlines_are_disabled() {
    let body = "private provider diagnostic ".repeat(100);
    let fixture = TlsFixture::start_streaming(
        vec![TestResponse {
            status: "400 Bad Request",
            content_type: "text/plain",
            body: &body,
            declared_length: None,
            delay: Duration::ZERO,
        }],
        Duration::from_millis(80),
    );
    let directory = HttpErrorLogDirectory::new();
    let mut settings = config(&fixture);
    settings.connect_timeout = Duration::from_millis(250);
    settings.total_timeout = Duration::from_millis(250);
    settings.idle_timeout = Duration::from_millis(250);
    let started = std::time::Instant::now();
    let failure = fixture_http_failure(&fixture, &directory, settings.without_deadlines());
    let elapsed = started.elapsed();
    assert!(
        elapsed < Duration::from_millis(900),
        "opening stage became unbounded: {elapsed:?}"
    );
    assert_eq!(fixture.finish().len(), 1);
    assert_eq!(failure.http_status, Some(400));
    assert!(!failure.retryable());
    let (metadata, bytes) = retained_http_error(&directory, failure);
    assert!(bytes.len() < body.len());
    assert_eq!(metadata["capture"]["complete"], false);
    assert!(metadata["capture"]["readFailure"].is_object());
}

#[test]
fn private_log_write_failure_keeps_http_status_and_permanent_stop() {
    use std::os::unix::fs::PermissionsExt as _;
    let fixture = TlsFixture::start(vec![TestResponse {
        status: "400 Bad Request",
        content_type: "application/json",
        body: PRIVATE_HTTP_ERROR,
        declared_length: None,
        delay: Duration::ZERO,
    }]);
    let directory = HttpErrorLogDirectory::new();
    std::fs::set_permissions(&directory.0, std::fs::Permissions::from_mode(0o755)).unwrap();
    let failure = fixture_http_failure(&fixture, &directory, config(&fixture));
    assert_eq!(fixture.finish().len(), 1);
    assert_eq!(failure.http_status, Some(400));
    assert!(!failure.retryable());
    let diagnostic = failure.diagnostic.unwrap();
    assert_eq!(
        diagnostic.response_log_status,
        Some(winwincode_network::ResponseLogStatus::WriteFailed)
    );
    assert_eq!(diagnostic.response_log, None);
    assert_eq!(std::fs::read_dir(&directory.0).unwrap().count(), 0);
    assert!(
        !serde_json::to_string(&failure)
            .unwrap()
            .contains(PRIVATE_HTTP_ERROR)
    );
}

#[test]
fn native_protocols_share_the_same_private_http_error_capture() {
    for protocol in 0..4 {
        let fixture = TlsFixture::start(vec![TestResponse {
            status: "400 Bad Request",
            content_type: "application/json",
            body: PRIVATE_HTTP_ERROR,
            declared_length: None,
            delay: Duration::ZERO,
        }]);
        let directory = HttpErrorLogDirectory::new();
        let settings = match protocol {
            0 => config(&fixture),
            1 => config(&fixture)
                .with_openai_chat_completions(8192, ProviderTokenPricing::default())
                .unwrap(),
            2 => config(&fixture)
                .with_anthropic_messages(8192, ProviderTokenPricing::default())
                .unwrap(),
            3 => config(&fixture).with_openai_responses_text(8192).unwrap(),
            _ => unreachable!(),
        };
        let adapter = HttpsSseProviderAdapter::try_new(settings)
            .unwrap()
            .with_sse_failure_log(directory.0.clone());
        let exchange = ModelExchangeId("mdl_00000000000000000000000001".into());
        let request_id = RequestId("req_00000000000000000000000001".into());
        let payload = serde_json::to_vec(&serde_json::json!({"requestId":"req-fixture", "provider":"p", "sessionId":"s", "threadId":"t", "request":{"model":"fixture-model", "instructions":"Reply briefly",
            "input":[{"type":"message","role":"user","content":[{"type":"input_text","text":"hello"}]}], "stream":true,
            "store":false,"tool_choice":"none","parallel_tool_calls":false,"reasoning":{"effort":"max"}}})).unwrap();
        let mut request = invocation(&exchange, &request_id);
        request.payload = &payload;
        let failure = adapter
            .open_https(request, SECRET)
            .unwrap_err()
            .network_failure();
        assert_eq!(
            failure.http_status,
            Some(400),
            "protocol {protocol} must reach the local HTTP boundary"
        );
        assert_eq!(fixture.finish().len(), 1);
        assert_eq!(
            retained_http_error(&directory, failure).1,
            PRIVATE_HTTP_ERROR.as_bytes()
        );
    }
}
