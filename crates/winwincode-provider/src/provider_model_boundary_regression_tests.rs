// SPDX-License-Identifier: Apache-2.0
thread_local! {
    static AUDIT_READER_READS: std::cell::RefCell<Vec<(usize,usize)>> = const { std::cell::RefCell::new(Vec::new()) };
}
pub(super) fn audit_record_reader_read(accepted: usize, read: usize) {
    AUDIT_READER_READS.with(|x| x.borrow_mut().push((accepted, read)));
}
fn audit_provider_report(name: &str, value: &serde_json::Value) {
    println!("MODEL_MECHANISM_AUDIT {value}");
    if let Some(root) = std::env::var_os("WWC_MECHANISM_AUDIT_OUTPUT") {
        let root = std::path::Path::new(&root);
        std::fs::create_dir_all(root).unwrap();
        std::fs::write(
            root.join(format!("{name}.json")),
            serde_json::to_vec_pretty(value).unwrap(),
        )
        .unwrap();
    }
}
fn audit_generic_receipt(adapter: &HttpsSseProviderAdapter) -> ProviderGatewayOpenReceipt {
    let exchange = ModelExchangeId("mdl_00000000000000000000000001".into());
    let request_id = RequestId("req_00000000000000000000000001".into());
    let mut request = invocation(&exchange, &request_id);
    let native_payload = responses_api_payload();
    if matches!(
        adapter.shared.config.protocol,
        HttpsSseProviderProtocol::OpenAiResponses { .. }
    ) {
        request.payload = &native_payload;
    }
    adapter.open_https(request, SECRET).unwrap();
    let mut leak_gate = crate::CredentialLeakGate::default();
    leak_gate.track_secret(&ResolvedSecret::from_bytes(SECRET.to_vec()).unwrap());
    ProviderGatewayOpenReceipt {
        model_exchange_id: exchange,
        request_id,
        route: winwincode_api::generated::ModelRoute {
            provider_id: "provider-https-fixture".into(),
            model_id: "fixture-model".into(),
            credential_reference_id: winwincode_domain::CredentialReferenceId(
                "crd_00000000000000000000000001".into(),
            ),
        },
        adapter_request_id: "pad_00000000000000000000000001".into(),
        idempotent_replay: false,
        stream_leak_gate: leak_gate,
    }
}

#[test]
#[ignore = "mechanism audit: conditional SSE terminal-to-HTTP EOF waiting cost baseline"]
fn mechanism_actual_tls_final_waits_for_http_eof_while_heartbeats_arrive() {
    let _ = rustls::crypto::aws_lc_rs::default_provider().install_default();
    let CertifiedKey { cert, signing_key } =
        generate_simple_self_signed(vec!["localhost".into()]).unwrap();
    let server_config = Arc::new(
        ServerConfig::builder()
            .with_no_client_auth()
            .with_single_cert(
                vec![cert.der().clone()],
                PrivateKeyDer::Pkcs8(PrivatePkcs8KeyDer::from(signing_key.serialize_der())),
            )
            .unwrap(),
    );
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let endpoint = format!(
        "https://localhost:{}/v1/model",
        listener.local_addr().unwrap().port()
    );
    let (request_tx, requests) = mpsc::channel();
    let (prefix_tx, prefix_rx) = mpsc::channel();
    let (release_tx, release_rx) = mpsc::channel();
    let server = thread::spawn(move || {
        let (socket, _) = listener.accept().unwrap();
        socket
            .set_write_timeout(Some(Duration::from_secs(3)))
            .unwrap();
        let mut stream = StreamOwned::new(ServerConnection::new(server_config).unwrap(), socket);
        request_tx.send(read_http_request(&mut stream)).unwrap();
        write!(stream,"HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nTransfer-Encoding: chunked\r\nConnection: close\r\n\r\n").unwrap();
        let prefix = successful_sse();
        write!(stream, "{:x}\r\n{prefix}\r\n", prefix.len()).unwrap();
        stream.flush().unwrap();
        prefix_tx.send(()).unwrap();
        let heartbeat = ": idle heartbeat\n\n";
        let mut count = 0;
        loop {
            match release_rx.recv_timeout(Duration::from_millis(100)) {
                Ok(()) => break,
                Err(mpsc::RecvTimeoutError::Timeout) => {
                    write!(stream, "{:x}\r\n{heartbeat}\r\n", heartbeat.len()).unwrap();
                    stream.flush().unwrap();
                    count += 1;
                }
                Err(error) => panic!("fixture release dropped: {error}"),
            }
        }
        write!(stream, "0\r\n\r\n").unwrap();
        stream.flush().unwrap();
        count
    });
    let fixture = TlsFixture {
        endpoint,
        certificate_der: cert.der().to_vec(),
        requests,
        server: thread::spawn(|| {}),
    };
    let settings = config(&fixture).without_deadlines();
    let adapter = HttpsSseProviderAdapter::try_new(settings).unwrap();
    let receipt = audit_generic_receipt(&adapter);
    let (result_tx, result_rx) = mpsc::channel();
    let started = std::time::Instant::now();
    let drain = thread::spawn(move || {
        result_tx.send(adapter.drain_canonical(&receipt)).unwrap();
    });
    prefix_rx.recv_timeout(Duration::from_secs(2)).unwrap();
    let before_eof = result_rx.recv_timeout(Duration::from_millis(550));
    let blocked_ms = started.elapsed().as_secs_f64() * 1000.0;
    release_tx.send(()).unwrap();
    let heartbeat_count = server.join().unwrap();
    let pending_before_eof = matches!(&before_eof, Err(mpsc::RecvTimeoutError::Timeout));
    let result = match before_eof {
        Err(mpsc::RecvTimeoutError::Timeout) => {
            result_rx.recv_timeout(Duration::from_secs(2)).unwrap()
        }
        Ok(result) => result,
        Err(error) => panic!("actual adapter drain unexpectedly disconnected: {error}"),
    };
    drain.join().unwrap();
    let completion = result.unwrap();
    assert!(matches!(
        completion.terminal,
        ProviderGatewayTerminal::Completed { .. }
    ));
    assert!(
        pending_before_eof,
        "complete SSE terminal does not complete actual reader before HTTP EOF"
    );
    assert!(heartbeat_count >= 4);
    assert_eq!(fixture.finish().len(), 1);
    audit_provider_report(
        "actual-sse-final-eof",
        &serde_json::json!({"schema_version":1,"finding":"MP-C01",
        "complete_terminal_sent":true,"pending_after_terminal_ms":blocked_ms,"heartbeat_tail_frames":heartbeat_count,
        "actual_reader_completed_only_after_http_eof":true,"physical_requests":1,"final_preserved":true}),
    );
}

fn audit_responses_usage_prefix() -> String {
    responses_wire(&[
        serde_json::json!({"type":"response.created","response":{"id":"synthetic-usage-response","model":"fixture-model"}}),
        serde_json::json!({"type":"response.completed","response":{"id":"synthetic-usage-response","model":"fixture-model","usage":{"input_tokens":11,"output_tokens":5},"end_turn":true}}),
    ])
}

#[test]
#[ignore = "mechanism audit: known size-limit receipt-loss and retryability baseline"]
fn mechanism_actual_tls_size_limit_discards_already_read_usage_receipt() {
    let prefix = audit_responses_usage_prefix();
    let body = format!("{prefix}: {}\n\n", "x".repeat(1024));
    let fixture = TlsFixture::start_streaming(
        vec![TestResponse {
            status: "200 OK",
            content_type: "text/event-stream",
            body: &body,
            declared_length: None,
            delay: Duration::ZERO,
        }],
        Duration::from_millis(5),
    );
    let mut settings = config(&fixture);
    settings.max_response_bytes = prefix.len() + 64;
    settings.max_event_bytes = 256;
    let adapter =
        HttpsSseProviderAdapter::try_new(settings.with_openai_responses_text(32_768).unwrap())
            .unwrap();
    let receipt = audit_generic_receipt(&adapter);
    let observed = adapter
        .observed_receipt(prefix.as_bytes(), &receipt)
        .expect("valid complete usage prefix must be independently extractable");
    assert_eq!(observed.1.input_tokens, 11);
    assert_eq!(observed.1.output_tokens, 5);
    AUDIT_READER_READS.with(|x| x.borrow_mut().clear());
    let error = adapter.drain_canonical(&receipt).unwrap_err();
    let reads = AUDIT_READER_READS.with(|x| std::mem::take(&mut *x.borrow_mut()));
    assert_eq!(error.kind(), HttpsSseProviderErrorKind::SizeLimit);
    assert!(
        error.observed_receipt().is_none(),
        "current size-limit loses the known valid usage prefix"
    );
    assert!(error.response().is_none());
    assert!(
        error.retryable(),
        "current protocol_invalid policy retries size limits"
    );
    assert!(
        reads.last().unwrap().0 >= prefix.len(),
        "actual reader already retained complete usage prefix before crossing limit"
    );
    assert_eq!(fixture.finish().len(), 1);
    audit_provider_report(
        "actual-size-limit-usage",
        &serde_json::json!({"schema_version":1,"finding":"MP-C02",
        "limit_bytes":prefix.len()+64,"known_valid_usage_prefix_bytes":prefix.len(),"actual_reader_reads":reads,
        "prefix_usage":{"input_tokens":11,"output_tokens":5},"observed_receipt_preserved":false,
        "response_prefix_preserved":false,"retryable":error.retryable(),"physical_requests":1,
        "accounting_limit":"adapter receipt measured; durable Device accounting_chunks requires separate full Device configuration fixture"}),
    );
}

#[test]
#[ignore = "mechanism audit: known size-limit durable usage-accounting loss baseline"]
#[allow(
    clippy::too_many_lines,
    reason = "Keep the actual TLS response, Device accounting path, and original assertions in one audit fixture"
)]
fn mechanism_actual_tls_size_limit_device_accounting_omits_known_usage() {
    use base64::{Engine as _, engine::general_purpose::STANDARD};
    let prefix = audit_responses_usage_prefix();
    let body = format!("{prefix}: {}\n\n", "x".repeat(1024));
    let fixture = TlsFixture::start_streaming(
        vec![TestResponse {
            status: "200 OK",
            content_type: "text/event-stream",
            body: &body,
            declared_length: None,
            delay: Duration::ZERO,
        }],
        Duration::from_millis(5),
    );
    let root = std::env::temp_dir().join(format!(
        "wwc-device-size-limit-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    let store = crate::DeviceProviderStore::open(&root).unwrap();
    let fixture_open: serde_json::Value = serde_json::from_str(include_str!(
        "../../../tests/fixtures/contracts/execution-port.valid.json"
    ))
    .unwrap();
    let mut open: winwincode_execution_port::generated::ModelOpenMessage = serde_json::from_value(
        fixture_open["messages"]
            .as_array()
            .unwrap()
            .iter()
            .find(|x| x["kind"] == "model.open")
            .unwrap()
            .clone(),
    )
    .unwrap();
    let request=serde_json::json!({"provider":"provider-https-fixture","request":serde_json::from_slice::<serde_json::Value>(&responses_api_payload()).unwrap()["request"]}).to_string();
    open.request.data_base64 = STANDARD.encode(request.as_bytes());
    open.request.content_type = "application/json".into();
    open.request.payload_digest.0 = format!("sha256:{:x}", Sha256::digest(request.as_bytes()));
    let provider = serde_json::json!({"providerId":"provider-https-fixture","displayName":"offline fixture","endpoint":fixture.endpoint,
        "protocol":"openai_responses","modelIds":["fixture-model"],"enabled":true});
    store
        .connection
        .execute(
            "INSERT INTO providers VALUES(?1,?2,?3)",
            rusqlite::params!["provider-https-fixture", provider.to_string(), SECRET],
        )
        .unwrap();
    let factory_calls = std::cell::Cell::new(0);
    let authority_started = std::time::Instant::now();
    AUDIT_READER_READS.with(|x| x.borrow_mut().clear());
    let chunks = store
        .execute_model_using(
            &open,
            || authority_started.elapsed() < Duration::from_secs(2),
            |_, _| {
                factory_calls.set(factory_calls.get() + 1);
                let mut settings = config(&fixture);
                settings.max_response_bytes = prefix.len() + 64;
                settings.max_event_bytes = 256;
                HttpsSseProviderAdapter::try_new(
                    settings.with_openai_responses_text(32_768).unwrap(),
                )
                .map_err(|_| crate::DeviceProviderError)
            },
        )
        .unwrap();
    let reads = AUDIT_READER_READS.with(|x| std::mem::take(&mut *x.borrow_mut()));
    let (state,accounting,response_bytes,first_failure):(String,Option<String>,Option<Vec<u8>>,String)=store.connection.query_row(
        "SELECT state,accounting_chunks,response_bytes,failure_chunks FROM model_invocation_attempts WHERE exchange_id=?1 AND attempt_number=1",
        [&open.model_exchange_id.0],|r|Ok((r.get(0)?,r.get(1)?,r.get(2)?,r.get(3)?))).unwrap();
    let first_failure: serde_json::Value = serde_json::from_str(&first_failure).unwrap();
    assert_eq!(
        first_failure[0]["error"]["code"],
        "DEVICE_PROVIDER_RESPONSE_TOO_LARGE"
    );
    assert_eq!(first_failure[0]["error"]["retryable"], true);
    let attempt_count: i64 = store
        .connection
        .query_row(
            "SELECT COUNT(*) FROM model_invocation_attempts WHERE exchange_id=?1",
            [&open.model_exchange_id.0],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(state, "failed");
    assert!(accounting.is_none());
    assert!(response_bytes.is_none());
    assert!(factory_calls.get() >= 1);
    assert_eq!(fixture.finish().len(), 1);
    assert!(reads.last().unwrap().0 >= prefix.len());
    assert!(chunks.last().unwrap().is_final);
    assert_eq!(chunks.len(), 1);
    assert!(!chunks[0].error.as_ref().unwrap().retryable);
    assert_eq!(
        store
            .execute_model_using(
                &open,
                || false,
                |_, _| panic!("complete local failure replay must not invoke adapter")
            )
            .unwrap(),
        chunks
    );
    audit_provider_report(
        "actual-size-limit-device-accounting",
        &serde_json::json!({"schema_version":1,"finding":"MP-C02",
        "actual_path":"DeviceProviderStore::execute_model_using -> HttpsSseProviderAdapter::read_bounded -> actual model_invocation_attempts",
        "physical_requests":1,"factory_calls":factory_calls.get(),"attempt_state":state,"usage_prefix_bytes":prefix.len(),
        "actual_reader_reads":reads,"attempt_accounting_chunks_is_null":true,"attempt_response_bytes_is_null":true,
        "first_size_limit_retryable":first_failure[0]["error"]["retryable"],"attempt_record_count":attempt_count,
        "authority_bound_seconds":2,"terminal_error_retryable":false,"local_failure_replay_requests":0,"known_usage":{"input_tokens":11,"output_tokens":5}}),
    );
    drop(store);
    std::fs::remove_dir_all(root).unwrap();
}

// The listener and socket waits are bounded even when the second attempt never
// arrives. This helper is test-only; the actual Provider reader is unchanged.
fn audit_bounded_tls_responses(responses: Vec<TestResponse<'_>>) -> TlsFixture {
    let _ = rustls::crypto::aws_lc_rs::default_provider().install_default();
    let CertifiedKey { cert, signing_key } =
        generate_simple_self_signed(vec!["localhost".into()]).unwrap();
    let server_config = Arc::new(
        ServerConfig::builder()
            .with_no_client_auth()
            .with_single_cert(
                vec![cert.der().clone()],
                PrivateKeyDer::Pkcs8(PrivatePkcs8KeyDer::from(signing_key.serialize_der())),
            )
            .unwrap(),
    );
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    listener.set_nonblocking(true).unwrap();
    let endpoint = format!(
        "https://localhost:{}/v1/model",
        listener.local_addr().unwrap().port()
    );
    let responses = responses
        .into_iter()
        .map(|response| {
            (
                response.status,
                response.content_type,
                response.body.to_owned(),
                response.declared_length,
            )
        })
        .collect::<Vec<_>>();
    let (request_tx, requests) = mpsc::channel();
    let server = thread::spawn(move || {
        let deadline = std::time::Instant::now() + Duration::from_secs(20);
        for (status, content_type, body, declared_length) in responses {
            let socket = loop {
                match listener.accept() {
                    Ok((socket, _)) => break socket,
                    Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                        assert!(
                            std::time::Instant::now() < deadline,
                            "owned TLS listener did not receive both attempts within20s"
                        );
                        thread::sleep(Duration::from_millis(10));
                    }
                    Err(error) => panic!("owned TLS accept failed: {error}"),
                }
            };
            socket.set_nonblocking(false).unwrap();
            socket
                .set_read_timeout(Some(Duration::from_secs(2)))
                .unwrap();
            socket
                .set_write_timeout(Some(Duration::from_secs(2)))
                .unwrap();
            let mut stream = StreamOwned::new(
                ServerConnection::new(Arc::clone(&server_config)).unwrap(),
                socket,
            );
            request_tx.send(read_http_request(&mut stream)).unwrap();
            write_http_response(
                &mut stream,
                &TestResponse {
                    status,
                    content_type,
                    body: &body,
                    declared_length,
                    delay: Duration::ZERO,
                },
                Duration::ZERO,
            );
        }
    });
    TlsFixture {
        endpoint,
        certificate_der: cert.der().to_vec(),
        requests,
        server,
    }
}

#[test]
#[allow(
    clippy::too_many_lines,
    reason = "Keep both actual TLS attempts, fresh decoder behavior, and original isolation assertions in one fixture"
)]
fn mechanism_actual_tls_failed_partial_retries_with_a_fresh_decoder() {
    use base64::{Engine as _, engine::general_purpose::STANDARD};
    let first = responses_wire(&[
        serde_json::json!({"type":"response.created","response":{"id":"owned-first-response","model":"fixture-model"}}),
        serde_json::json!({"type":"response.output_item.added","item":{"type":"message","id":"owned-first-item","role":"assistant","content":[]}}),
        serde_json::json!({"type":"response.output_text.delta","item_id":"owned-first-item","output_index":0,"content_index":0,"delta":"FIRST_PARTIAL_ONLY"}),
    ]);
    let second = responses_wire(&[
        serde_json::json!({"type":"response.created","response":{"id":"owned-second-response","model":"fixture-model"}}),
        serde_json::json!({"type":"response.output_item.added","item":{"type":"message","id":"owned-second-item","role":"assistant","content":[],"phase":"final_answer"}}),
        serde_json::json!({"type":"response.output_text.delta","item_id":"owned-second-item","output_index":0,"content_index":0,"delta":"SECOND_COMPLETE_ONLY"}),
        serde_json::json!({"type":"response.output_item.done","item":{"type":"message","id":"owned-second-item","role":"assistant","phase":"final_answer","content":[{"type":"output_text","text":"SECOND_COMPLETE_ONLY"}]}}),
        serde_json::json!({"type":"response.completed","response":{"id":"owned-second-response","model":"fixture-model","status":"completed","error":null,"end_turn":true,"usage":{"input_tokens":11,"output_tokens":5,"total_tokens":16}}}),
    ]);
    let fixture = audit_bounded_tls_responses(vec![
        TestResponse {
            status: "200 OK",
            content_type: "text/event-stream",
            body: &first,
            declared_length: Some(first.len() + 32),
            delay: Duration::ZERO,
        },
        TestResponse {
            status: "200 OK",
            content_type: "text/event-stream",
            body: &second,
            declared_length: None,
            delay: Duration::ZERO,
        },
    ]);
    let root = std::env::temp_dir().join(format!(
        "wwc-device-failed-partial-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    let store = crate::DeviceProviderStore::open(&root).unwrap();
    let template: serde_json::Value = serde_json::from_str(include_str!(
        "../../../tests/fixtures/contracts/execution-port.valid.json"
    ))
    .unwrap();
    let mut open: winwincode_execution_port::generated::ModelOpenMessage = serde_json::from_value(
        template["messages"]
            .as_array()
            .unwrap()
            .iter()
            .find(|message| message["kind"] == "model.open")
            .unwrap()
            .clone(),
    )
    .unwrap();
    let request = serde_json::json!({"provider":"provider-https-fixture",
        "request":serde_json::from_slice::<serde_json::Value>(&responses_api_payload()).unwrap()["request"]}).to_string();
    open.request.data_base64 = STANDARD.encode(request.as_bytes());
    open.request.content_type = "application/json".into();
    open.request.payload_digest.0 = format!("sha256:{:x}", Sha256::digest(request.as_bytes()));
    let provider = serde_json::json!({"providerId":"provider-https-fixture","displayName":"owned offline partial-response fixture",
        "endpoint":fixture.endpoint,"protocol":"openai_responses","modelIds":["fixture-model"],"enabled":true});
    store
        .connection
        .execute(
            "INSERT INTO providers VALUES(?1,?2,?3)",
            rusqlite::params!["provider-https-fixture", provider.to_string(), SECRET],
        )
        .unwrap();
    let factory_calls = std::cell::Cell::new(0);
    let started = std::time::Instant::now();
    let chunks = store
        .execute_model_using(
            &open,
            || started.elapsed() < Duration::from_secs(20),
            |_, _| {
                factory_calls.set(factory_calls.get() + 1);
                HttpsSseProviderAdapter::try_new(
                    config(&fixture).with_openai_responses_text(32_768).unwrap(),
                )
                .map_err(|_| crate::DeviceProviderError)
            },
        )
        .unwrap();
    let elapsed_ms = started.elapsed().as_secs_f64() * 1000.0;
    let requests = fixture.finish();
    assert_eq!(
        requests.len(),
        2,
        "two actual owned HTTP requests are required"
    );
    assert_eq!(
        factory_calls.get(),
        2,
        "the second physical attempt constructs a fresh adapter"
    );
    let (first_state, first_bytes, first_failure): (String, Vec<u8>, String) = store.connection.query_row(
        "SELECT state,response_bytes,failure_chunks FROM model_invocation_attempts WHERE exchange_id=?1 AND attempt_number=1",
        [&open.model_exchange_id.0], |row| Ok((row.get(0)?,row.get(1)?,row.get(2)?))).unwrap();
    assert_eq!(first_state, "failed");
    assert_eq!(
        first_bytes,
        first.as_bytes(),
        "actual reader retained the first physical response prefix"
    );
    let first_failure: serde_json::Value = serde_json::from_str(&first_failure).unwrap();
    assert_eq!(first_failure[0]["error"]["retryable"], true);
    let first_network_json: String = store.connection.query_row(
        "SELECT failure_json FROM model_attempt_diagnostics WHERE exchange_id=?1 AND sequence=1",
        [&open.model_exchange_id.0], |row| row.get(0)).unwrap();
    let first_network: winwincode_network::NetworkFailure =
        serde_json::from_str(&first_network_json).unwrap();
    assert_eq!(
        first_network.kind,
        winwincode_network::ErrorKind::TransportInterrupted
    );
    assert_eq!(
        first_network.acceptance,
        winwincode_network::Acceptance::ResponseReceived
    );
    assert_eq!(first_network.phase, winwincode_network::Phase::ResponseBody);
    let second_state: String = store
        .connection
        .query_row(
            "SELECT state FROM model_invocation_attempts WHERE exchange_id=?1 AND attempt_number=2",
            [&open.model_exchange_id.0],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(second_state, "completed");
    assert!(chunks.last().unwrap().is_final);
    assert!(chunks.iter().all(|chunk| chunk.error.is_none()));
    assert_eq!(chunks.first().unwrap().sequence.0, 1);
    let returned_text = chunks
        .iter()
        .filter_map(|chunk| chunk.payload.as_ref())
        .map(|payload| String::from_utf8(STANDARD.decode(&payload.data_base64).unwrap()).unwrap())
        .collect::<String>();
    assert!(returned_text.contains("SECOND_COMPLETE_ONLY"));
    assert!(!returned_text.contains("FIRST_PARTIAL_ONLY"));
    assert!(!returned_text.contains("owned-first-response"));
    let request_facts = requests
        .iter()
        .enumerate()
        .map(|(index, request)| {
            let header_end = find_bytes(request, b"\r\n\r\n").unwrap();
            let headers = std::str::from_utf8(&request[..header_end]).unwrap();
            let key = headers
                .lines()
                .find_map(|line| {
                    let (name, value) = line.split_once(':')?;
                    name.eq_ignore_ascii_case("idempotency-key")
                        .then(|| value.trim().to_owned())
                })
                .unwrap();
            assert_eq!(
                key,
                format!("device-{}:attempt:{}", open.model_exchange_id.0, index + 1)
            );
            serde_json::json!({"attempt":index+1,"idempotency_key":key,
            "request_body_bytes":request.len()-header_end-4,
            "request_body_sha256":format!("{:x}",Sha256::digest(&request[header_end+4..]))})
        })
        .collect::<Vec<_>>();
    assert_eq!(
        store
            .execute_model_using(
                &open,
                || false,
                |_, _| panic!("complete retained response must not open another physical request")
            )
            .unwrap(),
        chunks
    );
    audit_provider_report(
        "actual-tls-failed-partial-fresh-decoder",
        &serde_json::json!({
        "schema_version":1,"scope":"attached positive control, not a new finding ID",
        "actual_path":"DeviceProviderStore::execute_model_using -> actual HttpsSseProviderAdapter/read_bounded -> fresh second adapter decoder",
        "physical_requests":requests.len(),"factory_calls":factory_calls.get(),"elapsed_ms":elapsed_ms,
        "authority_bound_seconds":20,"listener_total_accept_bound_seconds":20,"socket_read_write_bound_seconds":2,
        "first_response_body_declared_bytes":first.len()+32,"first_response_prefix_bytes_retained":first_bytes.len(),
        "first_response_prefix_sha256":format!("{:x}",Sha256::digest(&first_bytes)),"first_network":first_network,
        "first_attempt_state":first_state,"first_error_retryable":true,"second_attempt_state":second_state,
        "requests":request_facts,"returned_chunk_count":chunks.len(),"returned_first_sequence":1,
        "complete_final_preserved":true,"first_partial_content_in_returned_response":false,
        "complete_local_replay_requests":0,
        "limit":"owned server intentionally ends Content-Length body early; no external model or production stream is used"}),
    );
    drop(store);
    std::fs::remove_dir_all(root).unwrap();
}
