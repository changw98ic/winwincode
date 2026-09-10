// SPDX-License-Identifier: Apache-2.0

use std::{
    collections::BTreeMap,
    fmt::Write as _,
    io::{Read as _, Write as _},
    net::{TcpListener, TcpStream},
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, AtomicU64, Ordering},
    },
    thread,
    time::Duration,
};

use rcgen::{CertifiedKey, generate_simple_self_signed};
use rustls::{
    ServerConfig, ServerConnection, StreamOwned,
    pki_types::{PrivateKeyDer, PrivatePkcs8KeyDer},
};
use serde::Deserialize;
use sha2::{Digest, Sha256};
use winwincode_domain::{ArtifactId, Sha256Digest};
use winwincode_s3_artifact_adapter::{
    S3ArtifactAdapter, S3ArtifactEncryptionPort, S3ArtifactError, S3ArtifactErrorKind,
    S3ArtifactHeaders, S3ArtifactIdentityPort, S3ArtifactLimits, S3ArtifactMethod,
    S3ArtifactRequestContext, S3ArtifactResponseHeaders, S3ArtifactTimeouts,
    S3ArtifactTransportConfig,
};

const BUCKET: &str = "fixture-artifacts";
const PREFIX: &str = "fixture-scope";
const AUTHORIZATION: &str = "Fixture fixture-s3-identity";
const WRONG_AUTHORIZATION: &str = "Fixture wrong-s3-identity";
const ENCRYPTION_KEY: &str = "fixture-key-7";
const ARTIFACT_BYTES: &[u8] = b"S3_ARTIFACT_PROTOCOL_FIXTURE";

struct FixtureIdentity {
    authorization: &'static str,
    calls: AtomicU64,
}

impl FixtureIdentity {
    fn new(authorization: &'static str) -> Self {
        Self {
            authorization,
            calls: AtomicU64::new(0),
        }
    }
}

impl S3ArtifactIdentityPort for FixtureIdentity {
    fn authorize(
        &self,
        request: &S3ArtifactRequestContext<'_>,
    ) -> Result<S3ArtifactHeaders, S3ArtifactError> {
        assert!(request.url().starts_with("https://localhost:"));
        assert!(request.operation_id().starts_with("wwco_"));
        assert_eq!(request.payload_sha256().len(), 64);
        if matches!(
            request.method(),
            S3ArtifactMethod::Put | S3ArtifactMethod::Post
        ) {
            assert!(
                request
                    .policy_headers()
                    .iter()
                    .any(|(name, value)| name == "x-fixture-encryption" && value == ENCRYPTION_KEY)
            );
        }
        self.calls.fetch_add(1, Ordering::Relaxed);
        S3ArtifactHeaders::try_new(vec![(
            "authorization".to_owned(),
            self.authorization.to_owned(),
        )])
    }
}

struct FixtureEncryption;

impl S3ArtifactEncryptionPort for FixtureEncryption {
    fn request_headers(
        &self,
        request: &S3ArtifactRequestContext<'_>,
    ) -> Result<S3ArtifactHeaders, S3ArtifactError> {
        let headers = if matches!(
            request.method(),
            S3ArtifactMethod::Put | S3ArtifactMethod::Post
        ) {
            vec![("x-fixture-encryption".to_owned(), ENCRYPTION_KEY.to_owned())]
        } else {
            Vec::new()
        };
        S3ArtifactHeaders::try_new(headers)
    }

    fn verify_response(
        &self,
        request: &S3ArtifactRequestContext<'_>,
        response: &S3ArtifactResponseHeaders,
    ) -> Result<(), S3ArtifactError> {
        if matches!(
            request.method(),
            S3ArtifactMethod::Put | S3ArtifactMethod::Post
        ) {
            assert!(request.policy_headers().iter().any(|(name, value)| {
                name == "x-fixture-encryption" && value == ENCRYPTION_KEY
            }));
        } else {
            assert_eq!(request.policy_headers().iter().count(), 0);
        }
        if response.value("x-fixture-encryption") == Some(ENCRYPTION_KEY) {
            Ok(())
        } else {
            Err(
                S3ArtifactHeaders::try_new(vec![("host".to_owned(), "invalid".to_owned())])
                    .expect_err("reserved header produces a secret-safe error"),
            )
        }
    }
}

#[derive(Clone)]
struct PartValue {
    digest: String,
    bytes: Vec<u8>,
}

#[derive(Clone)]
struct ServiceReply {
    status: u16,
    content_type: Option<&'static str>,
    operation_id: Option<String>,
    checksum: Option<String>,
    content_range: Option<String>,
    encrypted: bool,
    body: Vec<u8>,
    drop_connection: bool,
}

impl ServiceReply {
    fn empty(status: u16, operation_id: Option<String>) -> Self {
        Self {
            status,
            content_type: None,
            operation_id,
            checksum: None,
            content_range: None,
            encrypted: false,
            body: Vec::new(),
            drop_connection: false,
        }
    }

    fn object(status: u16, operation_id: String, checksum: String, body: Vec<u8>) -> Self {
        Self {
            status,
            content_type: Some("application/octet-stream"),
            operation_id: Some(operation_id),
            checksum: Some(checksum),
            content_range: None,
            encrypted: true,
            body,
            drop_connection: false,
        }
    }
}

#[derive(Default)]
struct ServiceState {
    parts: BTreeMap<(String, u64), PartValue>,
    objects: BTreeMap<String, Vec<u8>>,
    receipts: BTreeMap<String, ServiceReply>,
    mutations: u64,
    drop_first_mutation_response: bool,
}

impl ServiceState {
    fn with_lost_first_mutation() -> Self {
        let mut state = Self::default();
        state.drop_first_mutation_response = true;
        state
    }

    fn apply(&mut self, request: HttpRequest) -> ServiceReply {
        let Some(operation_id) = request
            .header("x-winwincode-operation-id")
            .map(str::to_owned)
        else {
            return ServiceReply::empty(422, None);
        };
        if let Some(replay) = self.receipts.get(&operation_id) {
            return replay.clone();
        }
        if matches!(request.method.as_str(), "PUT" | "POST")
            && request.header("x-fixture-encryption") != Some(ENCRYPTION_KEY)
        {
            return ServiceReply::empty(422, Some(operation_id));
        }
        if request.method == "PUT" && request.path().contains("/uploads/") {
            return self.put_part(request, operation_id);
        }
        if request.method == "POST" && request.path().contains("/uploads/") {
            return self.complete(&request, operation_id);
        }
        if request.method == "GET" && request.path().contains("/objects/sha256/") {
            return self.read_object(&request, operation_id);
        }
        if request.method == "DELETE" && request.path().contains("/uploads/") {
            return self.abort(&request, operation_id);
        }
        if request.method == "DELETE" && request.path().contains("/objects/sha256/") {
            return self.delete_object(&request, operation_id);
        }
        ServiceReply::empty(404, Some(operation_id))
    }

    fn put_part(&mut self, request: HttpRequest, operation_id: String) -> ServiceReply {
        let Some(upload_id) = request.query("uploadId").map(str::to_owned) else {
            return ServiceReply::empty(422, Some(operation_id));
        };
        let Some(sequence) = request
            .query("partNumber")
            .and_then(|value| value.parse::<u64>().ok())
        else {
            return ServiceReply::empty(422, Some(operation_id));
        };
        let Some(digest) = request
            .header("x-amz-meta-winwincode-sha256")
            .map(str::to_owned)
        else {
            return ServiceReply::empty(422, Some(operation_id));
        };
        if sha256(&request.body) != digest {
            return ServiceReply::empty(412, Some(operation_id));
        }
        let key = (upload_id, sequence);
        if let Some(existing) = self.parts.get(&key) {
            return if existing.digest == digest && existing.bytes == request.body {
                ServiceReply::object(200, operation_id, digest, Vec::new())
            } else {
                ServiceReply::empty(409, Some(operation_id))
            };
        }
        self.parts.insert(
            key,
            PartValue {
                digest: digest.clone(),
                bytes: request.body,
            },
        );
        self.mutation_success(ServiceReply::object(200, operation_id, digest, Vec::new()))
    }

    fn complete(&mut self, request: &HttpRequest, operation_id: String) -> ServiceReply {
        let Some(upload_id) = request.query("uploadId").map(str::to_owned) else {
            return ServiceReply::empty(422, Some(operation_id));
        };
        let Ok(completion) = serde_json::from_slice::<CompleteWire>(&request.body) else {
            return ServiceReply::empty(422, Some(operation_id));
        };
        if request.header("x-amz-meta-winwincode-sha256") != Some(&completion.digest.0) {
            return ServiceReply::empty(422, Some(operation_id));
        }
        let Some(bytes) = self.assemble(&upload_id, completion.last_sequence) else {
            return ServiceReply::empty(409, Some(operation_id));
        };
        if bytes.len() as u64 != completion.size_bytes || sha256(&bytes) != completion.digest.0 {
            return ServiceReply::empty(412, Some(operation_id));
        }
        if let Some(existing) = self.objects.get(&completion.digest.0) {
            return if *existing == bytes {
                ServiceReply::object(200, operation_id, completion.digest.0, Vec::new())
            } else {
                ServiceReply::empty(409, Some(operation_id))
            };
        }
        self.objects.insert(completion.digest.0.clone(), bytes);
        self.parts
            .retain(|(candidate, _), _| candidate != &upload_id);
        self.mutation_success(ServiceReply::object(
            200,
            operation_id,
            completion.digest.0,
            Vec::new(),
        ))
    }

    fn assemble(&self, upload_id: &str, last_sequence: u64) -> Option<Vec<u8>> {
        let mut bytes = Vec::new();
        for sequence in 1..=last_sequence {
            bytes.extend_from_slice(&self.parts.get(&(upload_id.to_owned(), sequence))?.bytes);
        }
        Some(bytes)
    }

    fn read_object(&self, request: &HttpRequest, operation_id: String) -> ServiceReply {
        let Some(digest) = object_digest(request.path()) else {
            return ServiceReply::empty(422, Some(operation_id));
        };
        let Some(bytes) = self.objects.get(&digest).cloned() else {
            return ServiceReply::empty(404, Some(operation_id));
        };
        let Some(range) = request.header("range") else {
            return ServiceReply::object(200, operation_id, digest, bytes);
        };
        let Some((start, end)) = parse_range(range, bytes.len()) else {
            return ServiceReply::empty(416, Some(operation_id));
        };
        let body = bytes[start..=end].to_vec();
        let mut reply = ServiceReply::object(206, operation_id, digest, body);
        reply.content_range = Some(format!("bytes {start}-{end}/{}", bytes.len()));
        reply
    }

    fn abort(&mut self, request: &HttpRequest, operation_id: String) -> ServiceReply {
        let Some(upload_id) = request.query("uploadId").map(str::to_owned) else {
            return ServiceReply::empty(422, Some(operation_id));
        };
        let before = self.parts.len();
        self.parts
            .retain(|(candidate, _), _| candidate != &upload_id);
        if self.parts.len() == before {
            return ServiceReply::empty(404, Some(operation_id));
        }
        self.mutation_success(ServiceReply::empty(204, Some(operation_id)))
    }

    fn delete_object(&mut self, request: &HttpRequest, operation_id: String) -> ServiceReply {
        let Some(digest) = object_digest(request.path()) else {
            return ServiceReply::empty(422, Some(operation_id));
        };
        if let Some(mut bytes) = self.objects.remove(&digest) {
            bytes.fill(0);
            self.mutation_success(ServiceReply::empty(204, Some(operation_id)))
        } else {
            ServiceReply::empty(404, Some(operation_id))
        }
    }

    fn mutation_success(&mut self, mut reply: ServiceReply) -> ServiceReply {
        let operation_id = reply.operation_id.clone().expect("mutation operation id");
        self.receipts.insert(operation_id, reply.clone());
        self.mutations += 1;
        if self.drop_first_mutation_response {
            self.drop_first_mutation_response = false;
            reply.drop_connection = true;
        }
        reply
    }
}

impl Drop for ServiceState {
    fn drop(&mut self) {
        for part in self.parts.values_mut() {
            part.bytes.fill(0);
        }
        for bytes in self.objects.values_mut() {
            bytes.fill(0);
        }
        for reply in self.receipts.values_mut() {
            reply.body.fill(0);
        }
    }
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct CompleteWire {
    digest: Sha256Digest,
    last_sequence: u64,
    size_bytes: u64,
}

struct HttpRequest {
    method: String,
    target: String,
    headers: BTreeMap<String, String>,
    body: Vec<u8>,
}

impl HttpRequest {
    fn header(&self, name: &str) -> Option<&str> {
        self.headers.get(name).map(String::as_str)
    }

    fn path(&self) -> &str {
        self.target
            .split_once('?')
            .map_or(self.target.as_str(), |(path, _)| path)
    }

    fn query(&self, name: &str) -> Option<&str> {
        self.target
            .split_once('?')?
            .1
            .split('&')
            .find_map(|part| part.split_once('=').filter(|(key, _)| *key == name))
            .map(|(_, value)| value)
    }
}

#[derive(Clone, Copy)]
enum ServerBehavior {
    Service,
    Delay(Duration),
}

struct TlsS3Fixture {
    endpoint: String,
    certificate_der: Vec<u8>,
    stop: Arc<AtomicBool>,
    server: thread::JoinHandle<()>,
}

impl TlsS3Fixture {
    fn start(state: &Arc<Mutex<ServiceState>>, behavior: ServerBehavior) -> Self {
        let _ = rustls::crypto::aws_lc_rs::default_provider().install_default();
        let CertifiedKey { cert, signing_key } =
            generate_simple_self_signed(vec!["localhost".to_owned()])
                .expect("generate S3 TLS certificate");
        let private_key =
            PrivateKeyDer::Pkcs8(PrivatePkcs8KeyDer::from(signing_key.serialize_der()));
        let config = ServerConfig::builder()
            .with_no_client_auth()
            .with_single_cert(vec![cert.der().clone()], private_key)
            .expect("build S3 TLS server");
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind S3 TLS fixture");
        listener
            .set_nonblocking(true)
            .expect("nonblocking S3 listener");
        let address = listener.local_addr().expect("S3 TLS address");
        let stop = Arc::new(AtomicBool::new(false));
        let server_stop = Arc::clone(&stop);
        let server_state = Arc::clone(state);
        let server = thread::spawn(move || {
            serve_tls(
                &listener,
                &Arc::new(config),
                &server_state,
                &server_stop,
                behavior,
            );
        });
        Self {
            endpoint: format!("https://localhost:{}", address.port()),
            certificate_der: cert.der().to_vec(),
            stop,
            server,
        }
    }

    fn finish(self) {
        self.stop.store(true, Ordering::Relaxed);
        self.server.join().expect("join S3 TLS fixture");
    }
}

fn serve_tls(
    listener: &TcpListener,
    config: &Arc<ServerConfig>,
    state: &Arc<Mutex<ServiceState>>,
    stop: &Arc<AtomicBool>,
    behavior: ServerBehavior,
) {
    while !stop.load(Ordering::Relaxed) {
        let socket = match listener.accept() {
            Ok((socket, _)) => socket,
            Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                thread::sleep(Duration::from_millis(2));
                continue;
            }
            Err(_) => return,
        };
        if socket.set_nonblocking(false).is_err() {
            continue;
        }
        let Ok(connection) = ServerConnection::new(Arc::clone(config)) else {
            continue;
        };
        let mut stream = StreamOwned::new(connection, socket);
        let Some(request) = read_http_request(&mut stream) else {
            continue;
        };
        let reply = if request.header("authorization") == Some(AUTHORIZATION) {
            match behavior {
                ServerBehavior::Service => state.lock().expect("S3 state lock").apply(request),
                ServerBehavior::Delay(delay) => {
                    thread::sleep(delay);
                    ServiceReply::empty(503, None)
                }
            }
        } else {
            ServiceReply::empty(401, None)
        };
        if !reply.drop_connection {
            write_http_response(&mut stream, &reply);
        }
    }
}

fn read_http_request(stream: &mut StreamOwned<ServerConnection, TcpStream>) -> Option<HttpRequest> {
    let mut bytes = Vec::new();
    let mut buffer = [0_u8; 4 * 1_024];
    loop {
        let count = stream.read(&mut buffer).ok()?;
        if count == 0 {
            return None;
        }
        bytes.extend_from_slice(&buffer[..count]);
        let Some(header_end) = find_bytes(&bytes, b"\r\n\r\n") else {
            continue;
        };
        let length = content_length(&bytes[..header_end]).unwrap_or(0);
        if bytes.len() >= header_end + 4 + length {
            return parse_http_request(&bytes, header_end, length);
        }
    }
}

fn parse_http_request(bytes: &[u8], header_end: usize, length: usize) -> Option<HttpRequest> {
    let headers = std::str::from_utf8(&bytes[..header_end]).ok()?;
    let mut lines = headers.lines();
    let mut request_line = lines.next()?.split_whitespace();
    let method = request_line.next()?.to_owned();
    let target = request_line.next()?.to_owned();
    let headers = lines
        .filter_map(|line| line.split_once(':'))
        .map(|(name, value)| (name.to_ascii_lowercase(), value.trim().to_owned()))
        .collect();
    Some(HttpRequest {
        method,
        target,
        headers,
        body: bytes[header_end + 4..header_end + 4 + length].to_vec(),
    })
}

fn write_http_response(
    stream: &mut StreamOwned<ServerConnection, TcpStream>,
    reply: &ServiceReply,
) {
    let reason = match reply.status {
        200 => "OK",
        204 => "No Content",
        206 => "Partial Content",
        401 => "Unauthorized",
        404 => "Not Found",
        409 => "Conflict",
        412 => "Precondition Failed",
        416 => "Range Not Satisfiable",
        503 => "Service Unavailable",
        _ => "Unprocessable Entity",
    };
    let mut header = format!(
        "HTTP/1.1 {} {}\r\nContent-Length: {}\r\nConnection: close\r\n",
        reply.status,
        reason,
        reply.body.len()
    );
    if let Some(value) = reply.content_type {
        let _ = write!(header, "Content-Type: {value}\r\n");
    }
    if let Some(value) = &reply.operation_id {
        let _ = write!(header, "X-WinWinCode-Operation-Id: {value}\r\n");
    }
    if let Some(value) = &reply.checksum {
        let _ = write!(header, "X-Amz-Meta-WinWinCode-Sha256: {value}\r\n");
    }
    if let Some(value) = &reply.content_range {
        let _ = write!(header, "Content-Range: {value}\r\n");
    }
    if reply.encrypted {
        let _ = write!(header, "X-Fixture-Encryption: {ENCRYPTION_KEY}\r\n");
    }
    header.push_str("\r\n");
    let _ = stream.write_all(header.as_bytes());
    let _ = stream.write_all(&reply.body);
    let _ = stream.flush();
}

fn content_length(headers: &[u8]) -> Option<usize> {
    std::str::from_utf8(headers)
        .ok()?
        .lines()
        .find_map(|line| {
            line.split_once(':')
                .filter(|(name, _)| name.eq_ignore_ascii_case("content-length"))
        })
        .and_then(|(_, value)| value.trim().parse().ok())
}

fn find_bytes(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    haystack
        .windows(needle.len())
        .position(|window| window == needle)
}

fn object_digest(path: &str) -> Option<String> {
    let suffix = path.split("/objects/sha256/").nth(1)?;
    let (head, tail) = suffix.split_once('/')?;
    let hex = format!("{head}{tail}");
    (head.len() == 2
        && hex.len() == 64
        && hex
            .bytes()
            .all(|byte| byte.is_ascii_digit() || matches!(byte, b'a'..=b'f')))
    .then(|| format!("sha256:{hex}"))
}

fn parse_range(value: &str, length: usize) -> Option<(usize, usize)> {
    let (start, end) = value.strip_prefix("bytes=")?.split_once('-')?;
    let start = start.parse::<usize>().ok()?;
    let end = end.parse::<usize>().ok()?;
    (start <= end && end < length).then_some((start, end))
}

fn sha256(bytes: &[u8]) -> String {
    format!("sha256:{:x}", Sha256::digest(bytes))
}

fn new_artifact_id(seed: u64) -> ArtifactId {
    ArtifactId(format!("art_{seed:026}"))
}

fn object_url(fixture: &TlsS3Fixture, digest: &Sha256Digest) -> String {
    let hex = digest.0.strip_prefix("sha256:").expect("fixture digest");
    format!(
        "{}/{}/{}/objects/sha256/{}/{}",
        fixture.endpoint,
        BUCKET,
        PREFIX,
        &hex[..2],
        &hex[2..]
    )
}

fn upload_url(fixture: &TlsS3Fixture, artifact_id: &ArtifactId) -> String {
    format!(
        "{}/{}/{}/uploads/{}",
        fixture.endpoint, BUCKET, PREFIX, artifact_id.0
    )
}

fn build_adapter(
    fixture: &TlsS3Fixture,
    identity: Arc<FixtureIdentity>,
    response_timeout: Duration,
    attempts: u8,
) -> S3ArtifactAdapter {
    let config = S3ArtifactTransportConfig::try_new(
        S3ArtifactTimeouts {
            connect: Duration::from_secs(2),
            response: response_timeout,
            total: Duration::from_secs(3),
        },
        S3ArtifactLimits {
            max_part_bytes: 1_024 * 1_024,
            max_object_bytes: 8 * 1_024 * 1_024,
            max_control_response_bytes: 64 * 1_024,
            max_attempts: attempts,
        },
    )
    .expect("S3 transport config")
    .with_specific_tls_roots(vec![fixture.certificate_der.clone()])
    .expect("S3 fixture TLS root");
    let identity: Arc<dyn S3ArtifactIdentityPort> = identity;
    let encryption: Arc<dyn S3ArtifactEncryptionPort> = Arc::new(FixtureEncryption);
    S3ArtifactAdapter::try_new(config, identity, encryption).expect("S3 adapter")
}

#[test]
fn tls_retry_multipart_range_digest_and_cleanup_are_one_neutral_contract() {
    let state = Arc::new(Mutex::new(ServiceState::with_lost_first_mutation()));
    let fixture = TlsS3Fixture::start(&state, ServerBehavior::Service);
    let identity = Arc::new(FixtureIdentity::new(AUTHORIZATION));
    let adapter = build_adapter(&fixture, Arc::clone(&identity), Duration::from_secs(1), 2);
    let artifact_id = new_artifact_id(7);
    let digest = Sha256Digest(sha256(ARTIFACT_BYTES));
    let split = ARTIFACT_BYTES.len() / 2;
    let upload = upload_url(&fixture, &artifact_id);
    let object = object_url(&fixture, &digest);

    let first_digest = Sha256Digest(sha256(&ARTIFACT_BYTES[..split]));
    adapter
        .put_chunk(
            &upload,
            &artifact_id,
            1,
            &first_digest,
            &ARTIFACT_BYTES[..split],
        )
        .expect("first part retries after lost receipt");
    let second_digest = Sha256Digest(sha256(&ARTIFACT_BYTES[split..]));
    adapter
        .put_chunk(
            &upload,
            &artifact_id,
            2,
            &second_digest,
            &ARTIFACT_BYTES[split..],
        )
        .expect("second part");
    adapter
        .finalize(
            &upload,
            &artifact_id,
            2,
            &digest,
            ARTIFACT_BYTES.len() as u64,
        )
        .expect("multipart completion");
    assert_eq!(
        adapter.read(&object, &digest).expect("complete read"),
        Some(ARTIFACT_BYTES.to_vec())
    );
    let range = adapter
        .read_range(&object, &digest, 3, 8)
        .expect("range read")
        .expect("range exists");
    assert_eq!(range.bytes(), &ARTIFACT_BYTES[3..11]);
    assert_eq!(range.total_size(), ARTIFACT_BYTES.len() as u64);
    assert_eq!(range.digest(), &digest);
    adapter.delete(&object, &digest).expect("object cleanup");
    adapter.delete(&object, &digest).expect("cleanup replay");
    assert!(
        adapter
            .read(&object, &digest)
            .expect("deleted read")
            .is_none()
    );
    assert_eq!(state.lock().expect("S3 state").mutations, 4);
    assert!(identity.calls.load(Ordering::Relaxed) >= 8);

    let public = format!("{adapter:?}");
    assert!(!public.contains(AUTHORIZATION));
    assert!(!public.contains(BUCKET));
    assert!(!public.contains(PREFIX));
    assert!(!public.contains(ENCRYPTION_KEY));
    fixture.finish();
}

#[test]
fn changed_replay_abort_and_corruption_fail_closed() {
    let state = Arc::new(Mutex::new(ServiceState::default()));
    let fixture = TlsS3Fixture::start(&state, ServerBehavior::Service);
    let adapter = build_adapter(
        &fixture,
        Arc::new(FixtureIdentity::new(AUTHORIZATION)),
        Duration::from_secs(1),
        1,
    );
    let artifact_id = new_artifact_id(8);
    let upload = upload_url(&fixture, &artifact_id);
    let first = b"first multipart value";
    let first_digest = Sha256Digest(sha256(first));
    let wrong_digest = adapter
        .put_chunk(
            &upload,
            &artifact_id,
            1,
            &Sha256Digest(sha256(b"different bytes")),
            first,
        )
        .expect_err("local digest mismatch");
    assert_eq!(wrong_digest.kind(), S3ArtifactErrorKind::DigestMismatch);
    adapter
        .put_chunk(&upload, &artifact_id, 1, &first_digest, first)
        .expect("first part");
    adapter
        .put_chunk(&upload, &artifact_id, 1, &first_digest, first)
        .expect("exact replay");
    let changed = b"changed multipart value";
    let conflict = adapter
        .put_chunk(
            &upload,
            &artifact_id,
            1,
            &Sha256Digest(sha256(changed)),
            changed,
        )
        .expect_err("changed replay");
    assert_eq!(conflict.kind(), S3ArtifactErrorKind::Conflict);
    adapter.abort_upload(&upload, &artifact_id).expect("abort");
    adapter
        .abort_upload(&upload, &artifact_id)
        .expect("abort replay");

    let corrupt_id = new_artifact_id(9);
    let corrupt_upload = upload_url(&fixture, &corrupt_id);
    let corrupt_bytes = b"accepted then corrupted";
    let corrupt_digest = Sha256Digest(sha256(corrupt_bytes));
    adapter
        .put_chunk(
            &corrupt_upload,
            &corrupt_id,
            1,
            &corrupt_digest,
            corrupt_bytes,
        )
        .expect("corruption part");
    adapter
        .finalize(
            &corrupt_upload,
            &corrupt_id,
            1,
            &corrupt_digest,
            corrupt_bytes.len() as u64,
        )
        .expect("corruption completion");
    state
        .lock()
        .expect("S3 state")
        .objects
        .get_mut(&corrupt_digest.0)
        .expect("accepted object")[0] ^= 1;
    let corruption = adapter
        .read(&object_url(&fixture, &corrupt_digest), &corrupt_digest)
        .expect_err("corrupt object");
    assert_eq!(corruption.kind(), S3ArtifactErrorKind::DigestMismatch);
    fixture.finish();
}

#[test]
fn timeout_and_authorization_fail_closed() {
    let delayed_state = Arc::new(Mutex::new(ServiceState::default()));
    let delayed = TlsS3Fixture::start(
        &delayed_state,
        ServerBehavior::Delay(Duration::from_millis(100)),
    );
    let timeout = build_adapter(
        &delayed,
        Arc::new(FixtureIdentity::new(AUTHORIZATION)),
        Duration::from_millis(20),
        2,
    );
    let missing_digest = Sha256Digest(sha256(b"missing"));
    let timeout_error = timeout
        .read(&object_url(&delayed, &missing_digest), &missing_digest)
        .expect_err("bounded timeout");
    assert_eq!(timeout_error.kind(), S3ArtifactErrorKind::Transport);
    delayed.finish();

    let denied_state = Arc::new(Mutex::new(ServiceState::default()));
    let denied = TlsS3Fixture::start(&denied_state, ServerBehavior::Service);
    let unauthorized = build_adapter(
        &denied,
        Arc::new(FixtureIdentity::new(WRONG_AUTHORIZATION)),
        Duration::from_secs(1),
        1,
    );
    let denied_error = unauthorized
        .read(&object_url(&denied, &missing_digest), &missing_digest)
        .expect_err("wrong identity");
    assert_eq!(denied_error.kind(), S3ArtifactErrorKind::PermissionDenied);
    let public = format!("{unauthorized:?} {denied_error:?}");
    assert!(!public.contains(WRONG_AUTHORIZATION));
    denied.finish();
}

#[test]
fn deployment_values_are_call_inputs_and_policy_headers_are_bounded() {
    let reserved =
        S3ArtifactHeaders::try_new(vec![("host".to_owned(), "example.invalid".to_owned())])
            .expect_err("transport-owned header");
    assert_eq!(reserved.kind(), S3ArtifactErrorKind::Invalid);

    let state = Arc::new(Mutex::new(ServiceState::default()));
    let fixture = TlsS3Fixture::start(&state, ServerBehavior::Service);
    let adapter = build_adapter(
        &fixture,
        Arc::new(FixtureIdentity::new(AUTHORIZATION)),
        Duration::from_secs(1),
        1,
    );
    let digest = Sha256Digest(sha256(b"missing"));
    for invalid in [
        "http://localhost/object",
        "https://user@localhost/object",
        "https://localhost/object?bucket=hidden",
        "https://localhost/a/../object",
    ] {
        let error = adapter.read(invalid, &digest).expect_err("invalid route");
        assert_eq!(error.kind(), S3ArtifactErrorKind::Invalid);
    }
    fixture.finish();
}
