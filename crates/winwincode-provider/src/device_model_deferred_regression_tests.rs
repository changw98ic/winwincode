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
fn m13_first_retry_after_301_seconds_is_deferred_without_exhausting_four_attempts() {
    exercise_deferred(false);
}

#[test]
fn deferred_model_retry_reopens_with_same_identity_and_new_physical_attempt() {
    exercise_deferred(true);
}

#[allow(
    clippy::too_many_lines,
    reason = "Keep actual TLS failures, durable scheduling and crash/reopen receipt assertions in one fixture"
)]
fn exercise_deferred(crash_resume: bool) {
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
    let sent_payloads = Arc::new(Mutex::new(Vec::new()));
    let retained_payloads = Arc::clone(&sent_payloads);
    let server = std::thread::spawn(move || {
        let tls = Arc::new(tls);
        for attempt in 0..if crash_resume { 2 } else { 1 } {
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
                rustls::ServerConnection::new(Arc::clone(&tls)).unwrap(),
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
            let headers_end = request
                .windows(4)
                .position(|bytes| bytes == b"\r\n\r\n")
                .unwrap();
            retained_payloads
                .lock()
                .unwrap()
                .push(request[headers_end + 4..].to_vec());
            counted.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            if attempt == 0 {
                stream.write_all(b"HTTP/1.1 503 Service Unavailable\r\nContent-Type: application/json\r\nRetry-After: 301\r\nContent-Length: 2\r\nConnection: close\r\n\r\n{}").unwrap();
            } else {
                let body = concat!(
                    "data: {\"type\":\"response.started\",\"responseId\":\"deferred-response\",\"modelId\":\"fixture-model\"}\n\n",
                    "data: {\"type\":\"usage\",\"inputTokens\":3,\"outputTokens\":2,\"cacheWriteInputTokens\":0,\"reasoningOutputTokens\":0}\n\n",
                    "data: {\"type\":\"response.finished\",\"reason\":\"stop\"}\n\n"
                );
                write!(stream,"HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",body.len()).unwrap();
            }
            stream.flush().unwrap();
        }
    });
    let started = Instant::now();
    let factory_calls = std::cell::Cell::new(0);
    let deferred = std::cell::RefCell::new(None);
    let factory = |_: &winwincode_api::generated::DeviceProviderConfig,
                   _: std::collections::BTreeMap<String, String>| {
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
        HttpsSseProviderAdapter::try_new(config).map_err(|_| DeviceProviderError::StateInvalid)
    };
    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        store.execute_model_using(
        &open,
        || {
            let state: Option<(u32,u64,String)> = store.connection.query_row("SELECT policy_attempt,wake_ms,decision FROM model_retry_state WHERE exchange_id=?1",[&open.model_exchange_id.0],|row| { let wake: i64=row.get(1)?; Ok((row.get(0)?,u64::try_from(wake).map_err(|_|rusqlite::Error::IntegralValueOutOfRange(1,wake))?,row.get(2)?)) }).optional().unwrap();
            if let Some(state)=state {
                *deferred.borrow_mut()=Some(state);
                assert!(!crash_resume, "synthetic process loss after persistent deferred decision");
            }
            started.elapsed()<Duration::from_millis(200)
        },
        factory,
    )
    }));
    if crash_resume {
        assert!(result.is_err());
        let (next_attempt, wake_ms, action) = deferred.into_inner().unwrap();
        assert_eq!((next_attempt, action.as_str()), (2, "deferred_until"));
        assert!(wake_ms >= winwincode_network::journal::now_millis() + 300_000);
        assert_eq!(factory_calls.get(), 1);
        assert_eq!(sends.load(std::sync::atomic::Ordering::SeqCst), 1);
        assert!(store.model_start_recorded(&open).unwrap());
        let prepared: Vec<u8> = store
            .connection
            .query_row(
                "SELECT prepared_payload FROM exchanges WHERE exchange_id=?1",
                [&open.model_exchange_id.0],
                |row| row.get(0),
            )
            .unwrap();
        drop(store);
        let store = DeviceProviderStore::open(&root).unwrap();
        assert_eq!(
            store.model_retry_state(&open).unwrap(),
            Some((next_attempt, wake_ms))
        );
        store
            .connection
            .execute(
                "UPDATE model_retry_state SET wake_ms=0 WHERE exchange_id=?1",
                [&open.model_exchange_id.0],
            )
            .unwrap();
        let chunks = store.execute_model_using(&open, || true, factory).unwrap();
        let resumed_prepared: Vec<u8> = store
            .connection
            .query_row(
                "SELECT prepared_payload FROM exchanges WHERE exchange_id=?1",
                [&open.model_exchange_id.0],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(resumed_prepared, prepared);
        assert!(
            chunks
                .last()
                .is_some_and(|chunk| chunk.is_final && chunk.error.is_none())
        );
        assert!(
            chunks.iter().all(|chunk| chunk.lease == open.lease
                && chunk.model_exchange_id == open.model_exchange_id)
        );
        let physical:Vec<(i64,String)>=store.connection.prepare("SELECT attempt_number,state FROM model_invocation_attempts WHERE exchange_id=?1 ORDER BY attempt_number").unwrap().query_map([&open.model_exchange_id.0],|row|Ok((row.get(0)?,row.get(1)?))).unwrap().collect::<Result<Vec<_>,_>>().unwrap();
        assert_eq!(
            physical,
            vec![(1, "failed".into()), (2, "completed".into())]
        );
        let policies:Vec<i64>=store.connection.prepare("SELECT policy_attempt FROM model_attempt_diagnostics WHERE exchange_id=?1 ORDER BY sequence").unwrap().query_map([&open.model_exchange_id.0],|row|row.get(0)).unwrap().collect::<Result<Vec<_>,_>>().unwrap();
        assert_eq!(policies, vec![1, 2]);
        assert!(store.model_retry_state(&open).unwrap().is_none());
        assert_eq!(factory_calls.get(), 2);
        assert_eq!(
            store
                .execute_model_using(
                    &open,
                    || false,
                    |_, _| panic!("completed ACK replay must not request again")
                )
                .unwrap(),
            chunks
        );
        server.join().unwrap();
        assert_eq!(sends.load(std::sync::atomic::Ordering::SeqCst), 2);
        let sent_payloads = sent_payloads.lock().unwrap();
        assert_eq!(sent_payloads.len(), 2);
        assert_eq!(sent_payloads[0], sent_payloads[1]);
        drop(store);
        std::fs::remove_dir_all(root).unwrap();
        return;
    }
    let chunks = result.unwrap().unwrap();
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
    let (next_attempt, wake_ms, action) = deferred
        .into_inner()
        .expect("the actual Device queue must persist its deferred decision before waiting");
    assert_eq!(
        next_attempt, 2,
        "only the next policy attempt is reserved; no physical send has occurred"
    );
    assert_eq!(action, "deferred_until");
    assert!(wake_ms >= winwincode_network::journal::now_millis() + 300_000);
    assert_eq!(stop.as_deref(), Some("authority_ended"));
    assert_eq!(
        final_retryable,
        Some(false),
        "authority loss ends the request; it does not exhaust its retry budget"
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
