// SPDX-License-Identifier: Apache-2.0

//! A complete bounded HTTP exchange, including the response body, is one attempt.
use crate::{
    Acceptance, ErrorKind, NetworkFailure, Phase, Replay, RequestRetry, RetryFailure,
    transport::{ExchangeCancellation, ExchangeConnector, ExchangeIo},
};
use std::{
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    time::{Duration, Instant},
};
use ureq::unversioned::transport::{ConnectProxyConnector, Connector, RustlsConnector};

struct HttpAttemptError {
    failure: NetworkFailure,
    response: Option<Box<ureq::http::Response<Vec<u8>>>>,
}
struct Done<'a>(&'a AtomicBool);
impl Drop for Done<'_> {
    fn drop(&mut self) {
        self.0.store(true, Ordering::Release);
    }
}
impl RetryFailure for HttpAttemptError {
    fn retryable(&self) -> bool {
        self.failure.retryable()
    }
    fn network_failure(&self) -> NetworkFailure {
        self.failure
    }
    fn wait_for_connection(&self) -> bool {
        self.failure.wait_for_connection()
    }
}

/// Executes one prepared logical request. The callback must retain the same
/// method, headers, body and request identity on every invocation.
/// Non-success HTTP responses remain available to the protocol decoder after
/// the network policy stops. Unknown non-idempotent writes require reconciliation.
///
/// # Errors
/// Returns only bounded network facts. The authority callback must remain live.
pub fn execute_http(
    agent: &ureq::Agent,
    send_once: impl FnMut(&ureq::Agent) -> Result<ureq::http::Response<ureq::Body>, ureq::Error>,
    max_response_bytes: u64,
    replay: Replay,
    timeout: Duration,
    can_start: impl Fn() -> bool + Sync,
) -> Result<ureq::http::Response<Vec<u8>>, NetworkFailure> {
    execute_owned(
        agent,
        send_once,
        max_response_bytes,
        replay,
        timeout,
        can_start,
        true,
    )
}

/// One complete attempt for durable queue owners. It never starts a retry loop.
/// # Errors
/// Returns bounded transport facts or the unchanged final HTTP response.
pub fn execute_http_once(
    agent: &ureq::Agent,
    send_once: impl FnMut(&ureq::Agent) -> Result<ureq::http::Response<ureq::Body>, ureq::Error>,
    max_response_bytes: u64,
    timeout: Duration,
    can_start: impl Fn() -> bool + Sync,
) -> Result<ureq::http::Response<Vec<u8>>, NetworkFailure> {
    execute_owned(
        agent,
        send_once,
        max_response_bytes,
        Replay::ReconcileFirst,
        timeout,
        can_start,
        false,
    )
}

fn execute_owned(
    agent: &ureq::Agent,
    mut send_once: impl FnMut(&ureq::Agent) -> Result<ureq::http::Response<ureq::Body>, ureq::Error>,
    max_response_bytes: u64,
    replay: Replay,
    timeout: Duration,
    can_start: impl Fn() -> bool + Sync,
    retry: bool,
) -> Result<ureq::http::Response<Vec<u8>>, NetworkFailure> {
    let deadline = Instant::now() + timeout;
    let cancellation = ExchangeCancellation::default();
    let done = AtomicBool::new(false);
    std::thread::scope(|scope| {
        scope.spawn(|| {
            while !done.load(Ordering::Acquire) {
                if !can_start() || Instant::now() >= deadline {
                    cancellation.cancel();
                    break;
                }
                std::thread::sleep(Duration::from_millis(crate::defaults().authority_check_ms));
            }
        });
        let _done = Done(&done);
        let stopped = || {
            NetworkFailure::new(
                if can_start() {
                    ErrorKind::Timeout
                } else {
                    ErrorKind::AuthorityExpired
                },
                Acceptance::Unknown,
                Phase::ResponseHeaders,
            )
        };
        let mut attempt = |_| {
            let remaining = deadline.saturating_duration_since(Instant::now());
            let io = ExchangeIo::new(remaining, remaining);
            cancellation.attach(&io);
            let config = agent.config().clone();
            let tracked = ureq::Agent::with_parts(
                config,
                ConnectProxyConnector::default()
                    .chain(ExchangeConnector(Arc::clone(&io)))
                    .chain(RustlsConnector::default()),
                crate::transport::ExchangeResolver(Arc::clone(&io)),
            );
            let response = send_once(&tracked).map_err(|error| HttpAttemptError {
                failure: if cancellation.is_cancelled() {
                    stopped()
                } else {
                    crate::classify_ureq(
                        &error,
                        !io.connection_established(),
                        Phase::ResponseHeaders,
                    )
                },
                response: None,
            })?;
            read_attempt(response, &io, max_response_bytes, || {
                cancellation.is_cancelled().then(&stopped)
            })
        };
        let result = if retry {
            RequestRetry::new(crate::defaults().max_attempts, b"http")
                .for_replay(replay)
                .run_blocking(
                    || can_start() && Instant::now() < deadline,
                    || HttpAttemptError {
                        failure: stopped(),
                        response: None,
                    },
                    attempt,
                    |_, _| Ok(()),
                )
        } else if can_start() && Instant::now() < deadline {
            attempt(1)
        } else {
            Err(HttpAttemptError {
                failure: stopped(),
                response: None,
            })
        };
        match result {
            Ok(response) => Ok(response),
            Err(error) => error
                .response
                .map(|response| *response)
                .ok_or(error.failure),
        }
    })
}

fn read_attempt(
    response: ureq::http::Response<ureq::Body>,
    io: &ExchangeIo,
    max_response_bytes: u64,
    cancelled: impl Fn() -> Option<NetworkFailure>,
) -> Result<ureq::http::Response<Vec<u8>>, HttpAttemptError> {
    io.body_started();
    let status = response.status().as_u16();
    let retry_after = response
        .headers()
        .get("retry-after")
        .and_then(|value| value.to_str().ok())
        .and_then(|value| crate::retry_after(value, std::time::SystemTime::now()));
    let (parts, mut body) = response.into_parts();
    let bytes = body
        .with_config()
        .limit(max_response_bytes)
        .read_to_vec()
        .map_err(|error| HttpAttemptError {
            failure: if let Some(failure) = cancelled() {
                failure
            } else {
                let mut failure = crate::classify_ureq(&error, false, Phase::ResponseBody);
                failure.acceptance = Acceptance::ResponseReceived;
                failure.http_status = Some(status);
                failure
            },
            response: None,
        })?;
    let response = ureq::http::Response::from_parts(parts, bytes);
    if !(200..300).contains(&status) {
        return Err(HttpAttemptError {
            failure: NetworkFailure::http(status, retry_after),
            response: Some(Box::new(response)),
        });
    }
    Ok(response)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{Read, Write};
    use std::net::TcpListener;

    fn agent() -> ureq::Agent {
        ureq::Agent::config_builder()
            .http_status_as_error(false)
            .proxy(None)
            .max_redirects(0)
            .build()
            .into()
    }
    fn fixture(replies: Vec<&'static [u8]>) -> (String, std::thread::JoinHandle<Vec<Vec<u8>>>) {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let url = format!("http://{}/test", listener.local_addr().unwrap());
        let thread = std::thread::spawn(move || {
            replies
                .into_iter()
                .map(|reply| {
                    let (mut socket, _) = listener.accept().unwrap();
                    socket
                        .set_read_timeout(Some(Duration::from_secs(15)))
                        .unwrap();
                    let mut request = vec![];
                    let mut buffer = [0; 1024];
                    loop {
                        let count = socket.read(&mut buffer).unwrap();
                        request.extend_from_slice(&buffer[..count]);
                        if count == 0 || request.windows(4).any(|bytes| bytes == b"\r\n\r\n") {
                            break;
                        }
                    }
                    socket.write_all(reply).unwrap();
                    request
                })
                .collect()
        });
        (url, thread)
    }

    #[test]
    fn full_response_retry_discards_failed_body_and_keeps_request_bytes() {
        let (url, server) = fixture(vec![
            b"HTTP/1.1 200 OK\r\nContent-Length: 20\r\nConnection: close\r\n\r\npartial",
            b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\nConnection: close\r\n\r\nok",
        ]);
        let response = execute_http(
            &agent(),
            |agent| {
                agent
                    .get(&url)
                    .header("X-Request-Id", "stable-request")
                    .call()
            },
            1024,
            Replay::ReplayExact,
            Duration::from_secs(15),
            || true,
        )
        .unwrap();
        assert_eq!(response.body(), b"ok");
        let requests = server.join().unwrap();
        assert_eq!(requests.len(), 2);
        assert_eq!(requests[0], requests[1]);
    }

    #[test]
    fn unsafe_unknown_response_is_not_automatically_resent() {
        let (url,server) = fixture(vec![b"HTTP/1.1 503 Service Unavailable\r\nContent-Length: 0\r\nRetry-After: 600\r\nConnection: close\r\n\r\n"]);
        let response = execute_http(
            &agent(),
            |agent| agent.post(&url).send(b"fixed"),
            1024,
            Replay::ReconcileFirst,
            Duration::from_secs(2),
            || true,
        )
        .unwrap();
        assert_eq!(response.status(), 503);
        assert_eq!(server.join().unwrap().len(), 1);
    }

    #[test]
    fn active_body_read_cancellation_closes_the_socket() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let url = format!("http://{}/test", listener.local_addr().unwrap());
        let (opened, ready) = std::sync::mpsc::channel();
        let server = std::thread::spawn(move || {
            let (mut socket, _) = listener.accept().unwrap();
            socket
                .set_read_timeout(Some(Duration::from_secs(2)))
                .unwrap();
            let mut buffer = [0; 1024];
            let mut request = Vec::new();
            while !request.windows(4).any(|bytes| bytes == b"\r\n\r\n") {
                let count = socket.read(&mut buffer).unwrap();
                assert!(count > 0, "request must arrive before the response");
                request.extend_from_slice(&buffer[..count]);
                assert!(request.len() <= 16 * 1024);
            }
            socket
                .write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 200\r\n\r\n")
                .unwrap();
            opened.send(()).unwrap();
            socket.read(&mut buffer).unwrap_or(0)
        });
        let live = AtomicBool::new(true);
        std::thread::scope(|scope| {
            let cancellation_live = &live;
            scope.spawn(move || {
                ready.recv().unwrap();
                std::thread::sleep(Duration::from_millis(20));
                cancellation_live.store(false, Ordering::Release);
            });
            let started = Instant::now();
            let failure = execute_http(
                &agent(),
                |agent| agent.get(&url).call(),
                1024,
                Replay::ReplayExact,
                Duration::from_secs(10),
                || live.load(Ordering::Acquire),
            )
            .unwrap_err();
            assert_eq!(failure.kind, ErrorKind::AuthorityExpired);
            assert!(started.elapsed() < Duration::from_millis(700));
        });
        assert_eq!(server.join().unwrap(), 0);
    }
}
