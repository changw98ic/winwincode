// SPDX-License-Identifier: Apache-2.0
#[test]
fn refused_tcp_open_retains_connect_phase_and_not_sent_proof() {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let endpoint = format!("https://{}/v1/model", listener.local_addr().unwrap());
    drop(listener);
    let _ = rustls::crypto::aws_lc_rs::default_provider().install_default();
    let settings = HttpsSseProviderConfig::try_new(
        "refused-tcp-fixture".to_owned(),
        endpoint.clone(),
        HttpsSseProviderTimeouts {
            connect: Duration::from_millis(200),
            idle: Duration::from_millis(100),
            total: Duration::from_secs(1),
        },
        HttpsSseProviderLimits {
            response_bytes: 64 * 1024,
            event_bytes: 8 * 1024,
            events: 64,
        },
    )
    .unwrap();
    let io =
        crate::provider_transport::ExchangeIo::new(settings.connect_timeout, settings.idle_timeout);
    let agent = HttpsSseProviderAdapter::agent(&settings, Arc::clone(&io));
    let error = agent.post(&endpoint).send(PAYLOAD).unwrap_err();
    assert!(!io.connection_established());
    let failure = HttpsSseProviderAdapter::http_open_failure(&io, &error);
    assert_eq!(
        failure.kind,
        winwincode_network::ErrorKind::ConnectionUnavailable
    );
    assert_eq!(failure.acceptance, winwincode_network::Acceptance::NotSent);
    assert_eq!(failure.phase, winwincode_network::Phase::Connect);
    assert_eq!(
        failure.diagnostic.unwrap().io_kind,
        Some(winwincode_network::IoFailureKind::ConnectionRefused)
    );
}

type AuditReaderReadAction = Box<dyn FnMut(usize, usize)>;

thread_local! {
    static AUDIT_READER_READS: std::cell::RefCell<Vec<(usize,usize)>> = const { std::cell::RefCell::new(Vec::new()) };
    static AUDIT_CACHE_PUBLICATION_ACTION: std::cell::RefCell<Option<Box<dyn FnMut()>>> = const { std::cell::RefCell::new(None) };
    static AUDIT_READER_READ_ACTION: std::cell::RefCell<Option<AuditReaderReadAction>> = const { std::cell::RefCell::new(None) };
}
pub(super) fn audit_record_reader_read(accepted: usize, read: usize) {
    AUDIT_READER_READS.with(|x| x.borrow_mut().push((accepted, read)));
    AUDIT_READER_READ_ACTION.with(|x| {
        if let Some(action) = x.borrow_mut().as_mut() {
            action(accepted, read);
        }
    });
}
pub(super) fn audit_before_cached_publication() {
    AUDIT_CACHE_PUBLICATION_ACTION.with(|action| {
        if let Some(action) = action.borrow_mut().as_mut() {
            action();
        }
    });
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

#[allow(
    clippy::too_many_lines,
    reason = "Keep the actual TLS exchange, pre-EOF wait and thread cleanup in one bounded fixture"
)]
fn audit_tls_terminal_boundary(
    prefix: &str,
    suffix: &str,
    heartbeat_tail: bool,
    protocol: HttpsSseProviderProtocol,
) -> (
    bool,
    usize,
    Result<HttpsSseProviderCompletion, HttpsSseProviderError>,
) {
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
    let prefix = prefix.to_owned();
    let suffix = suffix.to_owned();
    let server = thread::spawn(move || {
        let (socket, _) = listener.accept().unwrap();
        socket
            .set_write_timeout(Some(Duration::from_secs(3)))
            .unwrap();
        let mut stream = StreamOwned::new(ServerConnection::new(server_config).unwrap(), socket);
        request_tx.send(read_http_request(&mut stream)).unwrap();
        write!(stream,"HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nTransfer-Encoding: chunked\r\nConnection: close\r\n\r\n").unwrap();
        write!(stream, "{:x}\r\n{prefix}\r\n", prefix.len()).unwrap();
        stream.flush().unwrap();
        prefix_tx.send(()).unwrap();
        let heartbeat = ": idle heartbeat\n\n";
        let mut count = 0;
        let writable = loop {
            match release_rx.recv_timeout(Duration::from_millis(100)) {
                Ok(()) => break true,
                Err(mpsc::RecvTimeoutError::Timeout) => {
                    if !heartbeat_tail {
                        continue;
                    }
                    if write!(stream, "{:x}\r\n{heartbeat}\r\n", heartbeat.len()).is_err()
                        || stream.flush().is_err()
                    {
                        break false;
                    }
                    count += 1;
                }
                Err(error) => panic!("fixture release dropped: {error}"),
            }
        };
        if writable {
            if !suffix.is_empty() {
                let _ = write!(stream, "{:x}\r\n{suffix}\r\n", suffix.len());
            }
            let _ = write!(stream, "0\r\n\r\n");
            let _ = stream.flush();
        }
        count
    });
    let fixture = TlsFixture {
        endpoint,
        certificate_der: cert.der().to_vec(),
        requests,
        server: thread::spawn(|| {}),
    };
    let mut settings = config(&fixture).without_deadlines();
    settings.protocol = protocol;
    let adapter = HttpsSseProviderAdapter::try_new(settings).unwrap();
    let receipt = audit_generic_receipt(&adapter);
    let (result_tx, result_rx) = mpsc::channel();
    let drain = thread::spawn(move || {
        result_tx.send(adapter.drain_canonical(&receipt)).unwrap();
    });
    prefix_rx.recv_timeout(Duration::from_secs(2)).unwrap();
    let before_eof = result_rx.recv_timeout(Duration::from_millis(550));
    let returned_before_eof = before_eof.is_ok();
    let _ = release_tx.send(());
    let heartbeat_count = server.join().unwrap();
    let result = match before_eof {
        Err(mpsc::RecvTimeoutError::Timeout) => {
            result_rx.recv_timeout(Duration::from_secs(2)).unwrap()
        }
        Ok(result) => result,
        Err(error) => panic!("actual adapter drain unexpectedly disconnected: {error}"),
    };
    drain.join().unwrap();
    assert_eq!(fixture.finish().len(), 1);
    (returned_before_eof, heartbeat_count, result)
}

#[test]
fn mechanism_actual_tls_final_returns_before_http_eof_while_heartbeats_arrive() {
    let (returned_before_eof, heartbeat_count, completion) = audit_tls_terminal_boundary(
        successful_sse(),
        "",
        true,
        HttpsSseProviderProtocol::Canonical,
    );
    assert!(matches!(
        completion.unwrap().terminal,
        ProviderGatewayTerminal::Completed { .. }
    ));
    assert!(
        returned_before_eof,
        "a complete SSE terminal must return before the fixture releases HTTP EOF"
    );
    audit_provider_report(
        "actual-sse-final-eof",
        &serde_json::json!({"schema_version":1,"finding":"MP-C01",
        "complete_terminal_sent":true,"heartbeat_tail_frames":heartbeat_count,
        "actual_reader_returned_before_http_eof":returned_before_eof,"physical_requests":1,"final_preserved":true}),
    );
}

#[test]
fn mechanism_actual_tls_partial_terminal_waits_for_complete_event() {
    let prefix = successful_sse().strip_suffix("\n\n").unwrap();
    let (returned_before_eof, _, completion) =
        audit_tls_terminal_boundary(prefix, "\n\n", false, HttpsSseProviderProtocol::Canonical);
    assert!(
        !returned_before_eof,
        "a terminal JSON payload without a complete SSE event must remain pending"
    );
    assert!(matches!(
        completion.unwrap().terminal,
        ProviderGatewayTerminal::Completed { .. }
    ));
}

#[test]
fn mechanism_actual_tls_nonterminal_waits_for_protocol_terminal() {
    let (prefix, terminal) = successful_sse().rsplit_once("data: ").unwrap();
    let suffix = format!("data: {terminal}");
    let (returned_before_eof, _, completion) =
        audit_tls_terminal_boundary(prefix, &suffix, true, HttpsSseProviderProtocol::Canonical);
    assert!(
        !returned_before_eof,
        "complete nonterminal events must remain pending until the protocol terminal"
    );
    assert!(matches!(
        completion.unwrap().terminal,
        ProviderGatewayTerminal::Completed { .. }
    ));
}

fn audit_responses_invalid_terminal_prefix() -> String {
    responses_wire(&[
        serde_json::json!({"type":"response.created","response":{"id":"synthetic-usage-response","model":"fixture-model"}}),
        serde_json::json!({"type":"response.completed","response":{"id":"synthetic-usage-response","model":"fixture-model","usage":{"input_tokens":11,"output_tokens":5},"end_turn":true}}),
    ])
}

fn audit_responses_usage_prefix() -> String {
    responses_wire(&[
        serde_json::json!({"type":"response.created","response":{"id":"synthetic-usage-response","model":"fixture-model","usage":{"input_tokens":11,"output_tokens":5}}}),
    ])
}

fn audit_responses_terminal_protocol() -> HttpsSseProviderProtocol {
    HttpsSseProviderProtocol::OpenAiResponses {
        max_output_tokens: 32_768,
        structured_output: ResponsesStructuredOutput::Text,
    }
}

fn audit_tls_cancel_after_last_response_read(
    action: Option<ProviderStreamControlAction>,
    complete_event: bool,
) {
    use std::sync::atomic::{AtomicBool, Ordering};
    let complete_body = responses_api_body("text('synthetic cancel window');");
    let body = if complete_event {
        complete_body.as_str()
    } else {
        complete_body.strip_suffix("\n\n").unwrap()
    };
    let fixture = TlsFixture::start(vec![TestResponse {
        status: "200 OK",
        content_type: "text/event-stream",
        body,
        declared_length: None,
        delay: Duration::ZERO,
    }]);
    let cancellation = Arc::new(crate::provider_transport::ExchangeCancellation::default());
    let adapter = HttpsSseProviderAdapter::try_new(
        config(&fixture).with_openai_responses_text(32_768).unwrap(),
    )
    .unwrap()
    .with_cancellation(Arc::clone(&cancellation));
    let receipt = audit_generic_receipt(&adapter);
    let control_adapter = adapter.clone();
    let control_receipt = receipt.clone();
    let response_bytes = body.len();
    let fired = Arc::new(AtomicBool::new(false));
    let fired_in_read = Arc::clone(&fired);
    AUDIT_READER_READ_ACTION.with(|x| {
        *x.borrow_mut() = Some(Box::new(move |accepted, read| {
            if accepted + read >= response_bytes && !fired_in_read.swap(true, Ordering::SeqCst) {
                if let Some(action) = action {
                    control_adapter
                        .control(
                            &control_receipt.model_exchange_id,
                            &control_receipt.adapter_request_id,
                            action,
                        )
                        .unwrap();
                } else {
                    cancellation.cancel();
                }
            }
        }));
    });
    let result = adapter.drain_canonical(&receipt);
    AUDIT_READER_READ_ACTION.with(|x| *x.borrow_mut() = None);
    assert_eq!(fixture.finish().len(), 1);
    assert!(
        fired.load(Ordering::SeqCst),
        "actual TLS last response read must trigger the controlled cancellation window"
    );
    let error = result
        .expect_err("cancellation after the real response read must deny completion delivery");
    assert_eq!(error.kind(), HttpsSseProviderErrorKind::Transport);
    let observed = error.observed_receipt().unwrap();
    assert_eq!(observed.1.input_tokens, 20);
    assert_eq!(observed.1.output_tokens, 7);
    assert_eq!(error.response_bytes_received(), response_bytes as u64);
    assert_eq!(error.response(), Some(body.as_bytes()));
}

#[test]
fn mechanism_actual_tls_terminal_read_cancel_denies_completion() {
    audit_tls_cancel_after_last_response_read(Some(ProviderStreamControlAction::Cancel), true);
}

#[test]
fn mechanism_actual_tls_terminal_read_release_denies_completion() {
    audit_tls_cancel_after_last_response_read(Some(ProviderStreamControlAction::Release), true);
}

#[test]
fn mechanism_actual_tls_terminal_read_external_cancellation_denies_completion() {
    audit_tls_cancel_after_last_response_read(None, true);
}

#[test]
fn mechanism_actual_tls_eof_read_external_cancellation_denies_completion() {
    audit_tls_cancel_after_last_response_read(None, false);
}

#[test]
fn mechanism_actual_tls_eof_read_control_cancellation_denies_completion() {
    audit_tls_cancel_after_last_response_read(Some(ProviderStreamControlAction::Cancel), false);
}

fn audit_tls_cached_completion_denied_after_cancellation(
    action: Option<ProviderStreamControlAction>,
) {
    static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let body = responses_api_body("text('synthetic cached cancellation window');");
    let fixture = TlsFixture::start(vec![TestResponse {
        status: "200 OK",
        content_type: "text/event-stream",
        body: &body,
        declared_length: None,
        delay: Duration::ZERO,
    }]);
    let journal_root = std::env::temp_dir().join(format!(
        "wwc-provider-cached-cancellation-{}-{}",
        std::process::id(),
        NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
    ));
    std::fs::create_dir(&journal_root).unwrap();
    let journal_path = journal_root.join("network.sqlite3");
    let journal = winwincode_network::journal::RequestJournal::open(&journal_path).unwrap();
    let cancellation = Arc::new(crate::provider_transport::ExchangeCancellation::default());
    let adapter = HttpsSseProviderAdapter::try_new(
        config(&fixture).with_openai_responses_text(32_768).unwrap(),
    )
    .unwrap()
    .with_journal(journal)
    .with_cancellation(Arc::clone(&cancellation));
    let exchange = ModelExchangeId("mdl_00000000000000000000000001".into());
    let request_id = RequestId("req_00000000000000000000000001".into());
    let receipt = responses_api_receipt(exchange, request_id);
    let payload = responses_api_payload();
    let request = ProviderAdapterInvocation {
        model_exchange_id: &receipt.model_exchange_id,
        request_id: &receipt.request_id,
        adapter_request_id: &receipt.adapter_request_id,
        model_id: "fixture-model",
        content_type: "application/json",
        payload: &payload,
    };
    let credential = ResolvedSecret::from_bytes(SECRET.to_vec()).unwrap();
    let ack = adapter
        .open_recovering(&request, &credential, &receipt, &|| true)
        .unwrap();
    assert_eq!(ack.adapter_request_id(), receipt.adapter_request_id);
    assert_eq!(fixture.finish().len(), 1);
    let connection = rusqlite::Connection::open(&journal_path).unwrap();
    let attempts: Vec<(i64, String)> = connection
        .prepare("SELECT sequence, outcome FROM network_request_attempts")
        .unwrap()
        .query_map([], |row| Ok((row.get(0)?, row.get(1)?)))
        .unwrap()
        .collect::<Result<_, _>>()
        .unwrap();
    assert_eq!(attempts, vec![(1, "completed".into())]);
    let charge_before = {
        let completions = adapter.shared.completions.lock().unwrap();
        let completion = completions.get(&receipt.adapter_request_id).unwrap();
        assert!(!completion.frames.is_empty());
        completion.terminal.charge().unwrap()
    };
    assert_eq!(charge_before.usage.input_tokens, 20);
    assert_eq!(charge_before.usage.output_tokens, 7);
    if let Some(action) = action {
        adapter
            .control(
                &receipt.model_exchange_id,
                &receipt.adapter_request_id,
                action,
            )
            .unwrap();
    } else {
        cancellation.cancel();
    }
    let result = adapter.drain_canonical(&receipt);
    let cached_after = adapter
        .shared
        .completions
        .lock()
        .unwrap()
        .contains_key(&receipt.adapter_request_id);
    drop(connection);
    drop(adapter);
    std::fs::remove_dir_all(journal_root).unwrap();
    let error = result.expect_err("cancellation must deny delivery of the real cached completion");
    assert_eq!(error.kind(), HttpsSseProviderErrorKind::Transport);
    assert!(
        !cached_after,
        "denied cached output must be consumed and released"
    );
    assert_eq!(error.failure_terminal().charge(), Some(charge_before));
    let observed = error.observed_receipt().unwrap();
    assert_eq!(observed.0, "resp-api");
    assert_eq!(observed.1, charge_before.usage);
    assert!(
        error.response().is_none(),
        "cached completion has no raw HTTP body"
    );
    assert_eq!(error.response_bytes_received(), 0);
}

#[test]
fn mechanism_actual_tls_cached_completion_cancel_denies_delivery() {
    audit_tls_cached_completion_denied_after_cancellation(Some(
        ProviderStreamControlAction::Cancel,
    ));
}

#[test]
fn mechanism_actual_tls_cached_completion_release_denies_delivery() {
    audit_tls_cached_completion_denied_after_cancellation(Some(
        ProviderStreamControlAction::Release,
    ));
}

#[test]
fn mechanism_actual_tls_cached_completion_external_cancellation_denies_delivery() {
    audit_tls_cached_completion_denied_after_cancellation(None);
}

fn audit_tls_cached_lifecycle_case(action: Option<ProviderStreamControlAction>, publication: bool) {
    use std::sync::atomic::{AtomicBool, Ordering};
    static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let body = responses_api_body("text('synthetic cached lifecycle');");
    let fixture = TlsFixture::start(vec![TestResponse {
        status: "200 OK",
        content_type: "text/event-stream",
        body: &body,
        declared_length: None,
        delay: Duration::ZERO,
    }]);
    let journal_root = std::env::temp_dir().join(format!(
        "wwc-provider-cached-lifecycle-{}-{}",
        std::process::id(),
        NEXT.fetch_add(1, Ordering::Relaxed)
    ));
    std::fs::create_dir(&journal_root).unwrap();
    let journal_path = journal_root.join("network.sqlite3");
    let journal = winwincode_network::journal::RequestJournal::open(&journal_path).unwrap();
    let cancellation = Arc::new(crate::provider_transport::ExchangeCancellation::default());
    let adapter = HttpsSseProviderAdapter::try_new(
        config(&fixture).with_openai_responses_text(32_768).unwrap(),
    )
    .unwrap()
    .with_journal(journal)
    .with_cancellation(Arc::clone(&cancellation));
    let receipt = responses_api_receipt(
        ModelExchangeId("mdl_00000000000000000000000001".into()),
        RequestId("req_00000000000000000000000001".into()),
    );
    let payload = responses_api_payload();
    let invocation = ProviderAdapterInvocation {
        model_exchange_id: &receipt.model_exchange_id,
        request_id: &receipt.request_id,
        adapter_request_id: &receipt.adapter_request_id,
        model_id: "fixture-model",
        content_type: "application/json",
        payload: &payload,
    };
    let fired = Arc::new(AtomicBool::new(false));
    if publication {
        let control_adapter = adapter.clone();
        let control_receipt = receipt.clone();
        let control_cancellation = Arc::clone(&cancellation);
        let control_fired = Arc::clone(&fired);
        AUDIT_CACHE_PUBLICATION_ACTION.with(|slot| {
            *slot.borrow_mut() = Some(Box::new(move || {
                assert!(!control_fired.swap(true, Ordering::SeqCst));
                if let Some(action) = action {
                    control_adapter
                        .control(
                            &control_receipt.model_exchange_id,
                            &control_receipt.adapter_request_id,
                            action,
                        )
                        .unwrap();
                } else {
                    control_cancellation.cancel();
                }
            }));
        });
    }
    let credential = ResolvedSecret::from_bytes(SECRET.to_vec()).unwrap();
    let opened = adapter.open_recovering(&invocation, &credential, &receipt, &|| true);
    AUDIT_CACHE_PUBLICATION_ACTION.with(|slot| *slot.borrow_mut() = None);
    assert_eq!(fixture.finish().len(), 1);
    audit_cached_lifecycle_after_open(
        &adapter,
        &receipt,
        &journal_path,
        action,
        publication,
        &fired,
        &opened,
    );
    drop(adapter);
    std::fs::remove_dir_all(journal_root).unwrap();
}

fn audit_cached_lifecycle_after_open(
    adapter: &HttpsSseProviderAdapter,
    receipt: &ProviderGatewayOpenReceipt,
    journal_path: &std::path::Path,
    action: Option<ProviderStreamControlAction>,
    publication: bool,
    fired: &std::sync::atomic::AtomicBool,
    opened: &Result<ProviderAdapterOpenReceipt, ProviderAdapterError>,
) {
    use std::sync::atomic::Ordering;
    let connection = rusqlite::Connection::open(journal_path).unwrap();
    let attempts: Vec<(i64, String)> = connection
        .prepare("SELECT sequence, outcome FROM network_request_attempts")
        .unwrap()
        .query_map([], |row| Ok((row.get(0)?, row.get(1)?)))
        .unwrap()
        .collect::<Result<_, _>>()
        .unwrap();
    assert_eq!(attempts, vec![(1, "completed".into())]);
    let (cached_before, charge_before) = {
        let cache = adapter.shared.completions.lock().unwrap();
        (
            cache.contains_key(&receipt.adapter_request_id),
            cache
                .get(&receipt.adapter_request_id)
                .and_then(|value| value.terminal.charge()),
        )
    };
    if !publication {
        assert!(opened.is_ok());
        let charge = charge_before.unwrap();
        assert_eq!(charge.usage.input_tokens, 20);
        assert_eq!(charge.usage.output_tokens, 7);
        adapter
            .control(
                &receipt.model_exchange_id,
                &receipt.adapter_request_id,
                action.unwrap(),
            )
            .unwrap();
    }
    let cached_after = adapter
        .shared
        .completions
        .lock()
        .unwrap()
        .contains_key(&receipt.adapter_request_id);
    audit_provider_report(
        "cached-lifecycle-window",
        &serde_json::json!({
            "publication": publication, "action": format!("{action:?}"), "physicalRequests": 1,
            "journalCompleted": true, "openAccepted": opened.is_ok(), "cachedBefore": cached_before,
            "cachedAfter": cached_after, "knownInputTokens": charge_before.map(|value| value.usage.input_tokens),
            "knownOutputTokens": charge_before.map(|value| value.usage.output_tokens),
        }),
    );
    if publication {
        assert!(fired.load(Ordering::SeqCst));
        assert!(
            !cached_after,
            "cancellation at actual completion publication must not retain successful output frames"
        );
        assert!(
            opened.is_err(),
            "cancelled publication must deny the successful open acknowledgement"
        );
        let mut foreign = receipt.clone();
        foreign.model_exchange_id = ModelExchangeId("mdl_00000000000000000000000002".into());
        let foreign_error = adapter.drain_canonical(&foreign).unwrap_err();
        assert_eq!(
            foreign_error.kind(),
            HttpsSseProviderErrorKind::IdentityConflict
        );
        assert!(foreign_error.observed_receipt().is_none());
        assert!(foreign_error.failure_terminal().charge().is_none());
        let error = adapter.drain_canonical(receipt).unwrap_err();
        assert_eq!(error.kind(), HttpsSseProviderErrorKind::Transport);
        let charge = error.failure_terminal().charge().unwrap();
        assert_eq!(charge.usage.input_tokens, 20);
        assert_eq!(charge.usage.output_tokens, 7);
        assert_eq!(error.observed_receipt().unwrap().0, "resp-api");
        assert!(error.response().is_none());
        assert_eq!(error.response_bytes_received(), 0);
    } else {
        assert!(
            !cached_after,
            "terminal control must release cached output even when nobody drains again"
        );
    }
    drop(connection);
}

#[test]
fn mechanism_actual_tls_cached_cancel_releases_output_without_later_drain() {
    audit_tls_cached_lifecycle_case(Some(ProviderStreamControlAction::Cancel), false);
}

#[test]
fn mechanism_actual_tls_cached_release_releases_output_without_later_drain() {
    audit_tls_cached_lifecycle_case(Some(ProviderStreamControlAction::Release), false);
}

#[test]
fn mechanism_actual_tls_cached_publication_cancel_denies_output() {
    audit_tls_cached_lifecycle_case(Some(ProviderStreamControlAction::Cancel), true);
}

#[test]
fn mechanism_actual_tls_cached_publication_release_denies_output() {
    audit_tls_cached_lifecycle_case(Some(ProviderStreamControlAction::Release), true);
}

#[test]
fn mechanism_actual_tls_cached_publication_external_cancellation_denies_output() {
    audit_tls_cached_lifecycle_case(None, true);
}

fn audit_tls_cached_bounded_receipt(kind: &str) {
    static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let (body, protocol, expected_response_id, expected_input, expected_output, expected_cost) =
        match kind {
            "overlong" => (
                responses_api_body("text('synthetic overlong id');")
                    .replace("resp-api", &"x".repeat(201)),
                audit_responses_terminal_protocol(),
                None,
                20,
                7,
                None,
            ),
            "escaped" => {
                let response_id = "synthetic-quote\"-slash\\-identity";
                let encoded = serde_json::to_string(response_id).unwrap();
                (
                    responses_api_body("text('synthetic escaped id');")
                        .replace("\"resp-api\"", &encoded),
                    audit_responses_terminal_protocol(),
                    Some(response_id.to_owned()),
                    20,
                    7,
                    None,
                )
            }
            "failed" => {
                let body = responses_wire(&[
                    serde_json::json!({"type":"response.created","response":{"id":"synthetic-failed-id","model":"observed-api-model"}}),
                    serde_json::json!({"type":"response.failed","response":{"id":"synthetic-failed-id","model":"observed-api-model",
                    "usage":{"input_tokens":20,"output_tokens":7},"error":{"code":"server_error","message":"synthetic failure"}}}),
                ]);
                (body, audit_responses_terminal_protocol(), None, 20, 7, None)
            }
            "cost" => (
                successful_sse().to_owned(),
                HttpsSseProviderProtocol::Canonical,
                Some("response-1".to_owned()),
                11,
                5,
                Some(19),
            ),
            _ => panic!("unknown bounded receipt fixture"),
        };
    let fixture = TlsFixture::start(vec![TestResponse {
        status: "200 OK",
        content_type: "text/event-stream",
        body: &body,
        declared_length: None,
        delay: Duration::ZERO,
    }]);
    let journal_root = std::env::temp_dir().join(format!(
        "wwc-provider-bounded-cache-{}-{}",
        std::process::id(),
        NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
    ));
    std::fs::create_dir(&journal_root).unwrap();
    let journal_path = journal_root.join("network.sqlite3");
    let journal = winwincode_network::journal::RequestJournal::open(&journal_path).unwrap();
    let mut settings = config(&fixture);
    settings.protocol = protocol;
    let adapter = HttpsSseProviderAdapter::try_new(settings)
        .unwrap()
        .with_journal(journal);
    let receipt = responses_api_receipt(
        ModelExchangeId("mdl_00000000000000000000000001".into()),
        RequestId("req_00000000000000000000000001".into()),
    );
    let payload = responses_api_payload();
    let invocation = ProviderAdapterInvocation {
        model_exchange_id: &receipt.model_exchange_id,
        request_id: &receipt.request_id,
        adapter_request_id: &receipt.adapter_request_id,
        model_id: "fixture-model",
        content_type: "application/json",
        payload: &payload,
    };
    let credential = ResolvedSecret::from_bytes(SECRET.to_vec()).unwrap();
    adapter
        .open_recovering(&invocation, &credential, &receipt, &|| true)
        .unwrap();
    assert_eq!(fixture.finish().len(), 1);
    audit_bounded_cached_receipt_after_open(
        &adapter,
        &receipt,
        &journal_path,
        expected_response_id.as_deref(),
        expected_input,
        expected_output,
        expected_cost,
    );
    drop(adapter);
    std::fs::remove_dir_all(journal_root).unwrap();
}

fn audit_bounded_cached_receipt_after_open(
    adapter: &HttpsSseProviderAdapter,
    receipt: &ProviderGatewayOpenReceipt,
    journal_path: &std::path::Path,
    expected_response_id: Option<&str>,
    expected_input: u64,
    expected_output: u64,
    expected_cost: Option<u64>,
) {
    let connection = rusqlite::Connection::open(journal_path).unwrap();
    let attempts: Vec<(i64, String)> = connection
        .prepare("SELECT sequence,outcome FROM network_request_attempts")
        .unwrap()
        .query_map([], |row| Ok((row.get(0)?, row.get(1)?)))
        .unwrap()
        .collect::<Result<_, _>>()
        .unwrap();
    assert_eq!(attempts, vec![(1, "completed".into())]);
    let charge_before = adapter
        .shared
        .completions
        .lock()
        .unwrap()
        .get(&receipt.adapter_request_id)
        .and_then(|value| value.terminal.charge())
        .unwrap();
    assert_eq!(charge_before.usage.input_tokens, expected_input);
    assert_eq!(charge_before.usage.output_tokens, expected_output);
    assert_eq!(charge_before.actual_cost_micros, expected_cost);
    let mut foreign = receipt.clone();
    foreign.model_exchange_id = ModelExchangeId("mdl_00000000000000000000000002".into());
    let unknown = adapter.drain_canonical(&foreign).unwrap_err();
    assert_eq!(unknown.kind(), HttpsSseProviderErrorKind::IdentityConflict);
    assert!(unknown.observed_receipt().is_none());
    assert!(unknown.failure_terminal().charge().is_none());
    assert!(
        adapter
            .shared
            .completions
            .lock()
            .unwrap()
            .contains_key(&receipt.adapter_request_id)
    );
    adapter
        .control(
            &receipt.model_exchange_id,
            &receipt.adapter_request_id,
            ProviderStreamControlAction::Release,
        )
        .unwrap();
    assert!(
        !adapter
            .shared
            .completions
            .lock()
            .unwrap()
            .contains_key(&receipt.adapter_request_id)
    );
    let error = adapter.drain_canonical(receipt).unwrap_err();
    assert_eq!(error.failure_terminal().charge(), Some(charge_before));
    assert_eq!(
        error.observed_receipt().map(|value| value.0.as_str()),
        expected_response_id
    );
    if let Some((response_id, usage)) = error.observed_receipt() {
        assert!(response_id.len() <= 200);
        assert_eq!(*usage, charge_before.usage);
    }
    assert!(error.response().is_none());
    assert_eq!(error.response_bytes_received(), 0);
    drop(connection);
}

#[test]
fn mechanism_actual_tls_cached_overlong_id_preserves_charge_without_identity() {
    audit_tls_cached_bounded_receipt("overlong");
}

#[test]
fn mechanism_actual_tls_cached_escaped_id_preserves_bounded_identity() {
    audit_tls_cached_bounded_receipt("escaped");
}

#[test]
fn mechanism_actual_tls_cached_failed_terminal_preserves_charge_without_identity() {
    audit_tls_cached_bounded_receipt("failed");
}

#[test]
fn mechanism_actual_tls_cached_terminal_preserves_actual_cost() {
    audit_tls_cached_bounded_receipt("cost");
}

#[test]
fn mechanism_actual_tls_responses_final_returns_before_http_eof() {
    let body = responses_api_body("text('synthetic completed response');");
    let (returned_before_eof, _, completion) =
        audit_tls_terminal_boundary(&body, "", true, audit_responses_terminal_protocol());
    assert!(returned_before_eof);
    assert!(matches!(
        completion.unwrap().terminal,
        ProviderGatewayTerminal::Completed { .. }
    ));
}

#[test]
fn mechanism_actual_tls_invalid_terminal_is_rejected_before_http_eof_with_known_usage() {
    let prefix = audit_responses_invalid_terminal_prefix();
    let (returned_before_eof, _, completion) =
        audit_tls_terminal_boundary(&prefix, "", true, audit_responses_terminal_protocol());
    assert!(returned_before_eof);
    let error = completion.unwrap_err();
    assert_eq!(error.kind(), HttpsSseProviderErrorKind::SseEvent);
    let observed = error.observed_receipt().unwrap();
    assert_eq!(observed.1.input_tokens, 11);
    assert_eq!(observed.1.output_tokens, 5);
    let received = error.response().unwrap();
    assert!(received.starts_with(prefix.as_bytes()));
    assert_eq!(error.response_bytes_received(), received.len() as u64);
}

#[test]
fn mechanism_actual_tls_terminal_candidate_keeps_received_suffix_validation() {
    let body = format!(
        "{}data: {{\"type\":\"text.delta\",\"index\":0,\"delta\":\"synthetic post-terminal output\"}}\n\n",
        successful_sse()
    );
    let (returned_before_eof, _, completion) =
        audit_tls_terminal_boundary(&body, "", true, HttpsSseProviderProtocol::Canonical);
    assert!(returned_before_eof);
    let error = completion.unwrap_err();
    assert_eq!(error.kind(), HttpsSseProviderErrorKind::SseEvent);
    assert!(error.response().unwrap().starts_with(body.as_bytes()));
}

fn audit_terminal_scanner(protocol: HttpsSseProviderProtocol) -> SseTerminalScanner {
    SseTerminalScanner {
        protocol,
        max_event_bytes: 1024,
        max_events: 16,
        line: Vec::new(),
        data: Vec::new(),
        data_seen: false,
        first_line: true,
        previous_cr: false,
        events: 0,
    }
}

#[test]
fn mechanism_sse_terminal_scanner_handles_split_bom_utf8_crlf_and_multiline_data() {
    let wire = "\u{feff}data: {\"type\":\"text.delta\",\"index\":0,\"delta\":\"世\"}\r\n\r\ndata: {\r\ndata: \"type\":\"response.finished\",\r\ndata: \"reason\":\"stop\"}\r\n\r\n";
    for width in [1, 2, 7] {
        let mut scanner = audit_terminal_scanner(HttpsSseProviderProtocol::Canonical);
        let mut terminals = 0;
        for chunk in wire.as_bytes().chunks(width) {
            terminals += usize::from(scanner.inspect_fresh(chunk).unwrap());
        }
        assert_eq!(terminals, 1, "fresh transport read width {width}");
    }
    for newline in ["\n", "\r", "\r\n"] {
        let mut scanner = audit_terminal_scanner(HttpsSseProviderProtocol::Canonical);
        let event =
            format!("data: {{\"type\":\"response.finished\",\"reason\":\"stop\"}}{newline}");
        assert!(!scanner.inspect_fresh(event.as_bytes()).unwrap());
        assert!(scanner.inspect_fresh(newline.as_bytes()).unwrap());
    }
}

#[test]
fn mechanism_sse_terminal_scanner_uses_explicit_protocol_terminals() {
    let options = AnthropicMessagesOptions {
        max_output_tokens: 8192,
        pricing: ProviderTokenPricing::default(),
    };
    let protocols = [
        (
            HttpsSseProviderProtocol::Canonical,
            vec![
                "response.finished",
                "response.failed",
                "response.cancelled",
                "response.disconnected",
            ],
        ),
        (
            HttpsSseProviderProtocol::AnthropicMessages(options),
            vec!["message_stop", "error"],
        ),
        (
            audit_responses_terminal_protocol(),
            vec![
                "response.completed",
                "response.failed",
                "response.incomplete",
                "error",
            ],
        ),
        (
            HttpsSseProviderProtocol::CodexChatgpt,
            vec![
                "response.completed",
                "response.failed",
                "response.incomplete",
                "error",
            ],
        ),
        (
            HttpsSseProviderProtocol::ChatgptPlan,
            vec![
                "response.completed",
                "response.failed",
                "response.incomplete",
                "error",
            ],
        ),
    ];
    for (protocol, kinds) in protocols {
        for kind in kinds {
            let mut scanner = audit_terminal_scanner(protocol);
            let wire = format!("data: {{\"type\":\"{kind}\"}}\n");
            assert!(!scanner.inspect_fresh(wire.as_bytes()).unwrap());
            assert!(scanner.inspect_fresh(b"\n").unwrap());
        }
    }
    let mut chat = audit_terminal_scanner(HttpsSseProviderProtocol::OpenAiChatCompletions(options));
    assert!(
        !chat
            .inspect_fresh(b"data: {\"choices\":[{\"finish_reason\":\"stop\"}]}\n\n")
            .unwrap()
    );
    assert!(
        !chat
            .inspect_fresh(b"data: {\"usage\":{\"prompt_tokens\":11,\"completion_tokens\":5}}\n\n")
            .unwrap()
    );
    assert!(!chat.inspect_fresh(b"data: [DONE]\n").unwrap());
    assert!(chat.inspect_fresh(b"\n").unwrap());
}

#[test]
fn mechanism_sse_terminal_scanner_preserves_line_data_and_event_bounds() {
    for wire in [
        format!(": {}\n", "x".repeat(33)),
        format!("data: {}\ndata: {}\n", "x".repeat(16), "x".repeat(16)),
    ] {
        let mut scanner = audit_terminal_scanner(HttpsSseProviderProtocol::Canonical);
        scanner.max_event_bytes = 32;
        assert_eq!(
            scanner.inspect_fresh(wire.as_bytes()),
            Err(HttpsSseProviderErrorKind::SizeLimit)
        );
    }
    let mut scanner = audit_terminal_scanner(HttpsSseProviderProtocol::Canonical);
    scanner.max_events = 1;
    assert!(!scanner.inspect_fresh(b"data: one\n\n").unwrap());
    assert_eq!(
        scanner.inspect_fresh(b"data: two\n\n"),
        Err(HttpsSseProviderErrorKind::SizeLimit)
    );
}

#[test]
fn mechanism_actual_tls_size_limit_preserves_already_read_usage_receipt() {
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
        Duration::from_millis(50),
    );
    let mut settings = config(&fixture);
    settings.max_response_bytes = prefix.len() + 64;
    settings.max_event_bytes = prefix.len();
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
        error.observed_receipt().is_some(),
        "size-limit must retain a known valid usage prefix"
    );
    assert!(
        error
            .response()
            .is_some_and(|bytes| bytes.starts_with(prefix.as_bytes()))
    );
    let observed = error.observed_receipt().unwrap();
    assert_eq!(observed.1.input_tokens, 11);
    assert_eq!(observed.1.output_tokens, 5);
    assert!(error.response_bytes_received() > (prefix.len() + 64) as u64);
    assert!(
        !error.retryable(),
        "the unchanged response size limit must terminate this attempt"
    );
    let (retained_before_read, fresh_read) = *reads.last().unwrap();
    let received = retained_before_read + fresh_read;
    assert!(received >= prefix.len());
    assert_eq!(
        u64::try_from(received).unwrap(),
        error.response_bytes_received()
    );
    assert_eq!(fixture.finish().len(), 1);
    audit_provider_report(
        "actual-size-limit-usage",
        &serde_json::json!({"schema_version":1,"finding":"MP-C02",
        "limit_bytes":prefix.len()+64,"known_valid_usage_prefix_bytes":prefix.len(),"actual_reader_reads":reads,
        "prefix_usage":{"input_tokens":11,"output_tokens":5},"observed_receipt_preserved":true,
        "response_prefix_preserved":true,"retryable":error.retryable(),"physical_requests":1,
        "accounting_limit":"adapter receipt measured; durable Device accounting_chunks requires separate full Device configuration fixture"}),
    );
}

#[test]
#[allow(
    clippy::too_many_lines,
    reason = "Keep the actual TLS response, Device accounting path, and original assertions in one audit fixture"
)]
fn mechanism_actual_tls_size_limit_device_accounting_preserves_known_usage() {
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
        Duration::from_millis(50),
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
                settings.max_event_bytes = prefix.len();
                HttpsSseProviderAdapter::try_new(
                    settings.with_openai_responses_text(32_768).unwrap(),
                )
                .map_err(|_| crate::DeviceProviderError::StateInvalid)
            },
        )
        .unwrap();
    let reads = AUDIT_READER_READS.with(|x| std::mem::take(&mut *x.borrow_mut()));
    let (state,accounting,response_bytes,first_failure):(String,Option<String>,Option<Vec<u8>>,String)=store.runtime.connection.query_row(
        "SELECT state,accounting_chunks,response_bytes,failure_chunks FROM model_invocation_attempts WHERE exchange_id=?1 AND attempt_number=1",
        [&open.model_exchange_id.0],|r|Ok((r.get(0)?,r.get(1)?,r.get(2)?,r.get(3)?))).unwrap();
    let first_failure: serde_json::Value = serde_json::from_str(&first_failure).unwrap();
    assert_eq!(
        first_failure[0]["error"]["code"],
        "DEVICE_PROVIDER_RESPONSE_TOO_LARGE"
    );
    assert_eq!(first_failure[0]["error"]["retryable"], false);
    let attempt_count: i64 = store
        .runtime
        .connection
        .query_row(
            "SELECT COUNT(*) FROM model_invocation_attempts WHERE exchange_id=?1",
            [&open.model_exchange_id.0],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(state, "failed");
    let accounting: serde_json::Value = serde_json::from_str(&accounting.unwrap()).unwrap();
    let accounted: serde_json::Value = serde_json::from_slice(
        &STANDARD
            .decode(accounting[0]["payload"]["dataBase64"].as_str().unwrap())
            .unwrap(),
    )
    .unwrap();
    assert_eq!(accounted["tokenUsage"]["inputTokens"], 11);
    assert_eq!(accounted["tokenUsage"]["outputTokens"], 5);
    assert!(response_bytes.is_none());
    let received: i64 = store.runtime.connection.query_row("SELECT response_bytes_received FROM model_invocation_attempts WHERE exchange_id=?1 AND attempt_number=1",[&open.model_exchange_id.0],|row|row.get(0)).unwrap();
    assert!(received > i64::try_from(prefix.len() + 64).unwrap());
    assert_eq!(factory_calls.get(), 1);
    assert_eq!(attempt_count, 1);
    assert_eq!(fixture.finish().len(), 1);
    let (retained_before_read, fresh_read) = *reads.last().unwrap();
    let observed_received = retained_before_read + fresh_read;
    assert!(observed_received >= prefix.len());
    assert_eq!(i64::try_from(observed_received).unwrap(), received);
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
        "actual_reader_reads":reads,"attempt_accounting_chunks_is_null":false,"attempt_response_bytes_is_null":false,"response_bytes_received":received,
        "first_size_limit_retryable":first_failure[0]["error"]["retryable"],"attempt_record_count":attempt_count,
        "authority_bound_seconds":2,"terminal_error_retryable":false,"local_failure_replay_requests":0,"known_usage":{"input_tokens":11,"output_tokens":5}}),
    );
    drop(store);
    std::fs::remove_dir_all(root).unwrap();
}

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
                .map_err(|_| crate::DeviceProviderError::StateInvalid)
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
    let (first_state, first_bytes, first_failure): (String, Option<Vec<u8>>, String) = store.runtime.connection.query_row(
        "SELECT state,response_bytes,failure_chunks FROM model_invocation_attempts WHERE exchange_id=?1 AND attempt_number=1",
        [&open.model_exchange_id.0], |row| Ok((row.get(0)?,row.get(1)?,row.get(2)?))).unwrap();
    assert_eq!(first_state, "failed");
    assert!(first_bytes.is_none());
    let received: i64 = store.runtime.connection.query_row(
        "SELECT response_bytes_received FROM model_invocation_attempts WHERE exchange_id=?1 AND attempt_number=1",
        [&open.model_exchange_id.0], |row| row.get(0)).unwrap();
    assert_eq!(received, i64::try_from(first.len()).unwrap());
    let first_failure: serde_json::Value = serde_json::from_str(&first_failure).unwrap();
    assert_eq!(first_failure[0]["error"]["retryable"], true);
    let first_network_json: String = store.runtime.connection.query_row(
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
        .runtime
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
        "actual_path":"ProviderRuntimeStore::execute_model_using -> actual HttpsSseProviderAdapter/read_bounded -> fresh second adapter decoder",
        "physical_requests":requests.len(),"factory_calls":factory_calls.get(),"elapsed_ms":elapsed_ms,
        "authority_bound_seconds":20,"listener_total_accept_bound_seconds":20,"socket_read_write_bound_seconds":2,
        "first_response_body_declared_bytes":first.len()+32,"first_response_prefix_bytes_received":received,
        "first_response_prefix_bytes_retained":first_bytes.as_ref().map_or(0,Vec::len),
        "first_response_prefix_sha256":first_bytes.as_ref().map(|bytes|format!("{:x}",Sha256::digest(bytes))),"first_network":first_network,
        "first_attempt_state":first_state,"first_error_retryable":true,"second_attempt_state":second_state,
        "requests":request_facts,"returned_chunk_count":chunks.len(),"returned_first_sequence":1,
        "complete_final_preserved":true,"first_partial_content_in_returned_response":false,
        "complete_local_replay_requests":0,
        "limit":"owned server intentionally ends Content-Length body early; no external model or production stream is used"}),
    );
    drop(store);
    std::fs::remove_dir_all(root).unwrap();
}
