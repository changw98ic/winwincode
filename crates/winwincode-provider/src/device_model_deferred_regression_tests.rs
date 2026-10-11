// SPDX-License-Identifier: Apache-2.0

use super::*;
use crate::{
    HttpsSseProviderAdapter, HttpsSseProviderConfig, HttpsSseProviderLimits,
    HttpsSseProviderTimeouts,
};
use std::{
    io::{Read as _, Write as _},
    sync::Arc,
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

#[test]
#[ignore = "mechanism audit: known red; first DeferredUntil exhausts the retry budget"]
#[allow(
    clippy::too_many_lines,
    reason = "保持原实际Device DeferredUntil/Stop单路径回归及断言"
)]
fn m13_first_retry_after_301_seconds_is_deferred_without_exhausting_four_attempts() {
    let root = std::env::temp_dir().join(format!(
        "wwc-mechanism-deferred-{}-{}",
        std::process::id(),
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    let store = DeviceProviderStore::open(&root).unwrap();
    let fixture: serde_json::Value = serde_json::from_str(include_str!(
        "../../../tests/fixtures/contracts/execution-port.valid.json"
    ))
    .unwrap();
    let mut open: ModelOpenMessage = serde_json::from_value(
        fixture["messages"]
            .as_array()
            .unwrap()
            .iter()
            .find(|message| message["kind"] == "model.open")
            .unwrap()
            .clone(),
    )
    .unwrap();
    let payload=serde_json::to_vec(&serde_json::json!({"provider":"deferred-fixture","request":{"model":"fixture-model","stream":true,"input":"SYNTHETIC_DEFERRED_INPUT"}})).unwrap();
    open.request.content_type = "application/json".into();
    open.request.data_base64 = STANDARD.encode(&payload);
    open.request.payload_digest.0 = format!("sha256:{:x}", Sha256::digest(&payload));
    let certified = rcgen::generate_simple_self_signed(vec!["localhost".into()]).unwrap();
    let certificate = certified.cert.der().clone();
    let key = rustls::pki_types::PrivatePkcs8KeyDer::from(certified.signing_key.serialize_der());
    let tls = rustls::ServerConfig::builder_with_provider(Arc::new(
        rustls::crypto::aws_lc_rs::default_provider(),
    ))
    .with_safe_default_protocol_versions()
    .unwrap()
    .with_no_client_auth()
    .with_single_cert(vec![certificate.clone()], key.into())
    .unwrap();
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    listener.set_nonblocking(true).unwrap();
    let endpoint = format!(
        "https://localhost:{}/v1/responses",
        listener.local_addr().unwrap().port()
    );
    let provider = serde_json::json!({"providerId":"deferred-fixture","displayName":"fixture","endpoint":endpoint,"protocol":"canonical","modelIds":["fixture-model"],"enabled":true});
    store
        .connection
        .execute(
            "INSERT INTO providers VALUES(?1,?2,?3)",
            params![
                "deferred-fixture",
                provider.to_string(),
                b"SYNTHETIC_DEFERRED_CREDENTIAL".as_slice()
            ],
        )
        .unwrap();
    let sends = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let counted = Arc::clone(&sends);
    let server = std::thread::spawn(move || {
        let deadline = Instant::now() + Duration::from_secs(10);
        let socket = loop {
            match listener.accept() {
                Ok((socket, _)) => break socket,
                Err(error)
                    if error.kind() == std::io::ErrorKind::WouldBlock
                        && Instant::now() < deadline =>
                {
                    std::thread::sleep(Duration::from_millis(1));
                }
                Err(error) => panic!("synthetic TLS request did not arrive: {error}"),
            }
        };
        socket.set_nonblocking(false).unwrap();
        socket
            .set_read_timeout(Some(Duration::from_secs(5)))
            .unwrap();
        socket
            .set_write_timeout(Some(Duration::from_secs(5)))
            .unwrap();
        let mut stream = rustls::StreamOwned::new(
            rustls::ServerConnection::new(Arc::new(tls)).unwrap(),
            socket,
        );
        let mut request = Vec::new();
        loop {
            let mut buffer = [0; 4096];
            let read = stream.read(&mut buffer).unwrap();
            assert_ne!(read, 0);
            request.extend_from_slice(&buffer[..read]);
            if let Some(end) = request.windows(4).position(|bytes| bytes == b"\r\n\r\n") {
                let headers = std::str::from_utf8(&request[..end]).unwrap();
                let length: usize = headers
                    .lines()
                    .find_map(|line| {
                        let (name, value) = line.split_once(':')?;
                        name.eq_ignore_ascii_case("content-length")
                            .then(|| value.trim().parse().unwrap())
                    })
                    .unwrap();
                if request.len() >= end + 4 + length {
                    break;
                }
            }
        }
        counted.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        stream.write_all(b"HTTP/1.1 503 Service Unavailable\r\nContent-Type: application/json\r\nRetry-After: 301\r\nContent-Length: 2\r\nConnection: close\r\n\r\n{}").unwrap();
        stream.flush().unwrap();
    });
    let started = Instant::now();
    let factory_calls = std::cell::Cell::new(0);
    let chunks = store
        .execute_model_using(
            &open,
            || started.elapsed() < Duration::from_secs(2),
            |_, _| {
                factory_calls.set(factory_calls.get() + 1);
                let config = HttpsSseProviderConfig::try_new(
                    "deferred-fixture".into(),
                    endpoint.clone(),
                    HttpsSseProviderTimeouts {
                        connect: Duration::from_secs(2),
                        idle: Duration::from_secs(2),
                        total: Duration::from_secs(5),
                    },
                    HttpsSseProviderLimits {
                        response_bytes: 4096,
                        event_bytes: 4096,
                        events: 32,
                    },
                )
                .unwrap()
                .with_specific_tls_roots(vec![certificate.to_vec()])
                .unwrap();
                HttpsSseProviderAdapter::try_new(config).map_err(|_| DeviceProviderError)
            },
        )
        .unwrap();
    server.join().unwrap();
    let (attempt,stop,failure):(i64,Option<String>,String)=store.connection.query_row("SELECT policy_attempt,stop_reason,failure_json FROM model_attempt_diagnostics WHERE exchange_id=?1 ORDER BY sequence DESC LIMIT 1",[&open.model_exchange_id.0],|row|Ok((row.get(0)?,row.get(1)?,row.get(2)?))).unwrap();
    let network: winwincode_network::NetworkFailure = serde_json::from_str(&failure).unwrap();
    let mut retry =
        winwincode_network::RequestRetry::new(4, open.model_exchange_id.0.as_bytes()).state();
    let decision = retry.decision_after(&network);
    let stored_failure:String=store.connection.query_row("SELECT failure_chunks FROM model_invocation_attempts WHERE exchange_id=?1 AND attempt_number=1",[&open.model_exchange_id.0],|row|row.get(0)).unwrap();
    let first: Vec<ModelChunkMessage> = serde_json::from_str(&stored_failure).unwrap();
    let final_retryable = chunks
        .last()
        .and_then(|chunk| chunk.error.as_ref())
        .map(|error| error.retryable);
    eprintln!(
        "MECHANISM_M13 {}",
        serde_json::json!({"factoryCalls":factory_calls.get(),"actualSends":sends.load(std::sync::atomic::Ordering::SeqCst),"elapsedMs":started.elapsed().as_millis(),"policyAttempt":attempt,"providerRetryAfterMs":network.retry_after_ms,"policyDecision":format!("{decision:?}"),"stopReason":stop,"physicalFailureRetryable":first[0].error.as_ref().unwrap().retryable,"returnedFinalRetryable":final_retryable,"root":root})
    );
    assert_eq!(factory_calls.get(), 1);
    assert_eq!(sends.load(std::sync::atomic::Ordering::SeqCst), 1);
    assert_eq!(attempt, 1);
    assert_eq!(network.retry_after_ms, Some(301_000));
    assert!(
        matches!(
            decision,
            winwincode_network::RetryDecision::DeferredUntil(_)
        ),
        "the real physical failure retains a deferred policy decision: {decision:?}"
    );
    assert!(first[0].error.as_ref().unwrap().retryable);
    assert!(
        started.elapsed() < Duration::from_secs(5),
        "fixture authority ends after two seconds; never wait 301 seconds"
    );
    assert_ne!(
        stop.as_deref(),
        Some("retry_budget_exhausted"),
        "first DeferredUntil is not exhaustion of the four-attempt budget"
    );
    assert_ne!(
        final_retryable,
        Some(false),
        "a deferred transient error must not become a permanent model error"
    );
    let replay = store
        .execute_model_using(
            &open,
            || false,
            |_, _| panic!("exact exchange replay cannot invoke the provider factory"),
        )
        .unwrap();
    assert_eq!(replay, chunks);
    drop(store);
    std::fs::remove_dir_all(root).unwrap();
}
