// SPDX-License-Identifier: Apache-2.0

use super::*;
use std::{
    io::{Read, Write},
    net::TcpListener,
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
    },
    thread,
    time::Instant,
};

struct LocalProxy {
    agent: ureq::Agent,
    requests: Arc<Mutex<Vec<String>>>,
    finished: Arc<AtomicBool>,
    thread: thread::JoinHandle<()>,
}

impl LocalProxy {
    fn start(responses: Vec<(u16, &'static str, Option<usize>)>) -> Self {
        let _ = rustls::crypto::aws_lc_rs::default_provider().install_default();
        let rcgen::CertifiedKey { cert, signing_key } =
            rcgen::generate_simple_self_signed(vec!["auth.openai.com".into()]).unwrap();
        let tls = rustls::ServerConfig::builder()
            .with_no_client_auth()
            .with_single_cert(
                vec![cert.der().clone()],
                rustls::pki_types::PrivateKeyDer::Pkcs8(
                    rustls::pki_types::PrivatePkcs8KeyDer::from(signing_key.serialize_der()),
                ),
            )
            .unwrap();
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let proxy = format!("http://{}", listener.local_addr().unwrap());
        listener.set_nonblocking(true).unwrap();
        let requests = Arc::new(Mutex::new(Vec::new()));
        let captured = Arc::clone(&requests);
        let finished = Arc::new(AtomicBool::new(false));
        let stop = Arc::clone(&finished);
        let tls = Arc::new(tls);
        let thread = thread::spawn(move || {
            let deadline = Instant::now() + Duration::from_secs(35);
            while !stop.load(Ordering::Acquire) && Instant::now() < deadline {
                let (mut socket, _) = match listener.accept() {
                    Ok(value) => value,
                    Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                        thread::sleep(Duration::from_millis(2));
                        continue;
                    }
                    Err(error) => panic!("local CONNECT accept: {error}"),
                };
                socket.set_nonblocking(false).unwrap();
                socket
                    .set_read_timeout(Some(Duration::from_secs(2)))
                    .unwrap();
                socket
                    .set_write_timeout(Some(Duration::from_secs(2)))
                    .unwrap();
                let connect = headers(&mut socket);
                assert!(
                    connect.starts_with("CONNECT auth.openai.com:443 "),
                    "unexpected proxy target"
                );
                socket
                    .write_all(b"HTTP/1.1 200 Connection Established\r\n\r\n")
                    .unwrap();
                let connection = rustls::ServerConnection::new(Arc::clone(&tls)).unwrap();
                let mut stream = rustls::StreamOwned::new(connection, socket);
                let request = headers(&mut stream);
                let length = request
                    .lines()
                    .find_map(|line| {
                        let (key, value) = line.split_once(':')?;
                        key.eq_ignore_ascii_case("content-length")
                            .then(|| value.trim().parse::<usize>().unwrap())
                    })
                    .unwrap_or(0);
                let mut body = vec![0; length];
                stream.read_exact(&mut body).unwrap();
                // Only the request line is retained. Synthetic grant bytes remain in memory.
                let index = {
                    let mut requests = captured.lock().unwrap();
                    requests.push(request.lines().next().unwrap().to_owned());
                    requests.len() - 1
                };
                let (status, body, declared) =
                    responses.get(index).copied().unwrap_or((503, "{}", None));
                let response = format!(
                    "HTTP/1.1 {status} Fixture\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                    declared.unwrap_or(body.len())
                );
                let _ = stream.write_all(response.as_bytes());
                let _ = stream.flush();
            }
        });
        let config = ureq::Agent::config_builder()
            .proxy(Some(ureq::Proxy::new(&proxy).unwrap()))
            .http_status_as_error(false)
            .max_redirects(0)
            .timeout_global(Some(Duration::from_secs(3)))
            .tls_config(
                ureq::tls::TlsConfig::builder()
                    .root_certs(ureq::tls::RootCerts::new_with_certs(&[
                        ureq::tls::Certificate::from_der(cert.der()).to_owned(),
                    ]))
                    .build(),
            )
            .build();
        Self {
            agent: config.into(),
            requests,
            finished,
            thread,
        }
    }

    fn finish(self) -> Vec<String> {
        self.finished.store(true, Ordering::Release);
        self.thread.join().unwrap();
        Arc::try_unwrap(self.requests)
            .unwrap()
            .into_inner()
            .unwrap()
    }
}

fn headers(stream: &mut impl Read) -> String {
    let mut bytes = Vec::new();
    while !bytes.ends_with(b"\r\n\r\n") {
        let mut byte = [0; 1];
        stream.read_exact(&mut byte).unwrap();
        bytes.push(byte[0]);
        assert!(bytes.len() < 16_384);
    }
    String::from_utf8(bytes).unwrap()
}

#[test]
fn rotating_grant_http_error_and_truncated_body_are_never_replayed() {
    for response in [(503, "{}", None), (200, "{", Some(64))] {
        let proxy = LocalProxy::start(vec![response]);
        assert!(
            token_request(
                &proxy.agent,
                &[
                    ("grant_type", "refresh_token"),
                    ("refresh_token", "synthetic-rotating-grant")
                ]
            )
            .is_err()
        );
        let requests = proxy.finish();
        assert_eq!(
            requests.len(),
            1,
            "an unknown rotating grant cannot be sent twice"
        );
        assert!(requests[0].starts_with("POST /api/accounts/oauth/token "));
    }
}

#[test]
fn read_only_identity_key_fetch_retries_transient_status_on_loopback_only() {
    let proxy = LocalProxy::start(vec![
        (503, "{}", None),
        (425, "{}", None),
        (200, "{\"keys\":[]}", None),
    ]);
    assert!(load_jwks(&proxy.agent).is_ok());
    let requests = proxy.finish();
    assert_eq!(requests.len(), 3);
    assert!(
        requests
            .iter()
            .all(|request| request.starts_with("GET /.well-known/jwks.json "))
    );
}

#[test]
fn oauth_json_rejects_non_success_and_oversize_even_when_payload_is_valid() {
    for status in [401, 429, 503] {
        let response = ureq::http::Response::builder()
            .status(status)
            .body(b"{\"keys\":[]}".to_vec())
            .unwrap();
        assert!(read_json::<JwkSet>(response).is_err());
    }
    let response = ureq::http::Response::builder()
        .status(200)
        .body(b"{\"keys\":[]}".to_vec())
        .unwrap();
    assert!(read_json::<JwkSet>(response).is_ok());
    let response = ureq::http::Response::builder()
        .status(200)
        .body(vec![b' '; 524_289])
        .unwrap();
    assert!(read_json::<JwkSet>(response).is_err());
}
