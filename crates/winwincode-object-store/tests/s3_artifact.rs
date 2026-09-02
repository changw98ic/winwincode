// SPDX-License-Identifier: Apache-2.0

use std::{
    collections::BTreeMap,
    fmt::Write as _,
    fs,
    io::{Read as _, Write as _},
    net::{TcpListener, TcpStream},
    path::{Path, PathBuf},
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
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use winwincode_audit::AuditScope;
use winwincode_backup::{
    BackupCaptureCoordinator, BackupComponentKind, BackupComponentSnapshot, BackupId,
    BackupSnapshotRequest, BackupSnapshotSource, BackupSnapshotSourceError,
};
use winwincode_domain::{
    ArtifactId, DeliveryId, ExecutionJobId, ExecutionMessageId, FencingToken, LeaseId,
    OrganizationId, ProductSessionId, ProjectId, RepositoryId, RequestId, Sha256Digest, UserId,
    WorkerId, WorkerInstanceId, WorkerSessionId, WorkspaceId,
};
use winwincode_object_store::{
    S3ArtifactClock, S3ArtifactClockError, S3ArtifactConfig, S3ArtifactEncryption,
    S3ArtifactLimits, S3ArtifactObjectStore, S3ArtifactTimeouts, S3ArtifactWorkloadCredential,
    S3ArtifactWorkloadIdentityPort, SystemS3ArtifactClock,
};
use winwincode_storage::{
    ArtifactAccess, ArtifactChunk, ArtifactError, ArtifactErrorKind, ArtifactMeteringAttribution,
    ArtifactObjectStore, ArtifactOpen, ArtifactProvenance, ArtifactRetention, ArtifactStore,
    ReceiptScopeKey,
};

const BUCKET: &str = "wwc-artifacts";
const PREFIX: &str = "tenant-a";
const WORKLOAD_TOKEN: &[u8] = b"s3-workload-token-fixture";
const ARTIFACT_MARKER: &[u8] = b"S3_ARTIFACT_RESTRICTED_MARKER";
const INVENTORY_SCHEMA: &str = "winwincode.s3-artifact-inventory.v1";
const KMS_KEY_REFERENCE: &str = "kms://object-store/key/version/7";

static NEXT_DIRECTORY: AtomicU64 = AtomicU64::new(1);

#[derive(Default)]
struct TestClock(AtomicU64);

impl TestClock {
    fn at(value: u64) -> Self {
        Self(AtomicU64::new(value))
    }
}

impl S3ArtifactClock for TestClock {
    fn now_ms(&self) -> Result<u64, S3ArtifactClockError> {
        Ok(self.0.load(Ordering::Relaxed))
    }
}

struct FixtureIdentity {
    token: Vec<u8>,
    clock: Arc<TestClock>,
}

impl S3ArtifactWorkloadIdentityPort for FixtureIdentity {
    fn issue(&self) -> Result<S3ArtifactWorkloadCredential, ArtifactError> {
        let now = self.clock.0.load(Ordering::Relaxed);
        S3ArtifactWorkloadCredential::try_new(self.token.clone(), now + 60_000)
    }
}

impl Drop for FixtureIdentity {
    fn drop(&mut self) {
        self.token.fill(0);
    }
}

struct LiveIdentity(Vec<u8>);

impl S3ArtifactWorkloadIdentityPort for LiveIdentity {
    fn issue(&self) -> Result<S3ArtifactWorkloadCredential, ArtifactError> {
        let now = SystemS3ArtifactClock
            .now_ms()
            .map_err(|_| ArtifactError::object_adapter(ArtifactErrorKind::Adapter))?;
        S3ArtifactWorkloadCredential::try_new(self.0.clone(), now + 5 * 60 * 1_000)
    }
}

impl Drop for LiveIdentity {
    fn drop(&mut self) {
        self.0.fill(0);
    }
}

struct TestDirectory(PathBuf);

impl TestDirectory {
    fn new(label: &str) -> Self {
        let sequence = NEXT_DIRECTORY.fetch_add(1, Ordering::Relaxed);
        let path = std::env::temp_dir().join(format!(
            "winwincode-s3-artifact-{label}-{}-{sequence}",
            std::process::id()
        ));
        let _ = fs::remove_dir_all(&path);
        fs::create_dir_all(&path).expect("create S3 Artifact fixture");
        Self(path)
    }
}

impl Drop for TestDirectory {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
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
    server_side_encryption: Option<&'static str>,
    kms_key_reference: Option<String>,
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
            server_side_encryption: None,
            kms_key_reference: None,
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
            server_side_encryption: Some("aws:kms"),
            kms_key_reference: Some(KMS_KEY_REFERENCE.to_owned()),
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
    object_reads: u64,
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
        if matches!(request.method.as_str(), "PUT" | "POST") && !valid_encryption_headers(&request)
        {
            return ServiceReply::empty(422, Some(operation_id));
        }
        if request.method == "PUT" && request.path().contains("/uploads/") {
            return self.put_part(request, operation_id);
        }
        if request.method == "POST" && request.path().contains("/uploads/") {
            return self.complete(&request, operation_id);
        }
        if request.method == "GET" && request.path() == format!("/{BUCKET}") {
            return self.inventory(&request, operation_id);
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

    fn read_object(&mut self, request: &HttpRequest, operation_id: String) -> ServiceReply {
        let Some(digest) = object_digest(request.path()) else {
            return ServiceReply::empty(422, Some(operation_id));
        };
        let Some(bytes) = self.objects.get(&digest).cloned() else {
            return ServiceReply::empty(404, Some(operation_id));
        };
        self.object_reads += 1;
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

    fn inventory(&mut self, request: &HttpRequest, operation_id: String) -> ServiceReply {
        let Some(cut) = request.query("consistency-cut") else {
            return ServiceReply::empty(422, Some(operation_id));
        };
        let Some(scope) = request.query("scope-digest") else {
            return ServiceReply::empty(422, Some(operation_id));
        };
        let cut = Sha256Digest(format!("sha256:{cut}"));
        let scope = Sha256Digest(format!("sha256:{scope}"));
        let Ok((content, count, bytes)) = self.inventory_facts() else {
            return ServiceReply::empty(412, Some(operation_id));
        };
        let checkpoint = sha256(
            &[
                cut.0.as_bytes(),
                scope.0.as_bytes(),
                content.0.as_bytes(),
                &count.to_be_bytes(),
                &bytes.to_be_bytes(),
            ]
            .concat(),
        );
        let body = serde_json::to_vec(&InventoryWire {
            schema: INVENTORY_SCHEMA.to_owned(),
            operation_id: operation_id.clone(),
            consistency_cut_digest: cut,
            scope_digest: scope,
            checkpoint_digest: Sha256Digest(checkpoint),
            content_digest: content,
            record_count: count,
            byte_count: bytes,
        })
        .expect("canonical inventory response");
        let reply = ServiceReply {
            status: 200,
            content_type: Some("application/json"),
            operation_id: Some(operation_id.clone()),
            checksum: None,
            content_range: None,
            server_side_encryption: None,
            kms_key_reference: None,
            body,
            drop_connection: false,
        };
        self.receipts.insert(operation_id, reply.clone());
        reply
    }

    fn inventory_facts(&self) -> Result<(Sha256Digest, u64, u64), ()> {
        let mut input = Vec::new();
        let mut byte_count = 0_u64;
        for (digest, bytes) in &self.objects {
            if sha256(bytes) != *digest {
                return Err(());
            }
            input.extend_from_slice(digest.as_bytes());
            input.extend_from_slice(&(bytes.len() as u64).to_be_bytes());
            byte_count = byte_count.checked_add(bytes.len() as u64).ok_or(())?;
        }
        let count = u64::try_from(self.objects.len()).map_err(|_| ())?;
        Ok((Sha256Digest(sha256(&input)), count, byte_count))
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

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct InventoryWire {
    schema: String,
    operation_id: String,
    consistency_cut_digest: Sha256Digest,
    scope_digest: Sha256Digest,
    checkpoint_digest: Sha256Digest,
    content_digest: Sha256Digest,
    record_count: u64,
    byte_count: u64,
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
        let reply = if request
            .header("authorization")
            .is_some_and(valid_workload_authorization)
        {
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

fn valid_workload_authorization(value: &str) -> bool {
    value
        .as_bytes()
        .strip_prefix(b"Bearer ")
        .is_some_and(|token| token == WORKLOAD_TOKEN)
}

fn valid_encryption_headers(request: &HttpRequest) -> bool {
    request.header("x-amz-server-side-encryption") == Some("aws:kms")
        && request.header("x-amz-server-side-encryption-aws-kms-key-id") == Some(KMS_KEY_REFERENCE)
        && request
            .header("x-amz-meta-winwincode-encryption-context-sha256")
            .is_some_and(|value| value == sha256(b"tenant-a-encryption-context"))
}

fn read_http_request(stream: &mut StreamOwned<ServerConnection, TcpStream>) -> Option<HttpRequest> {
    let mut bytes = Vec::new();
    let mut buffer = [0_u8; 4 * 1024];
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
    if let Some(value) = reply.server_side_encryption {
        let _ = write!(header, "X-Amz-Server-Side-Encryption: {value}\r\n");
    }
    if let Some(value) = &reply.kms_key_reference {
        let _ = write!(
            header,
            "X-Amz-Server-Side-Encryption-Aws-Kms-Key-Id: {value}\r\n"
        );
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

fn id(prefix: &str, seed: u64) -> String {
    format!("{prefix}_{seed:026}")
}

fn artifact_scope(value: &str) -> ReceiptScopeKey {
    ReceiptScopeKey::from_encoded(value.as_bytes().to_vec()).expect("Artifact scope")
}

fn audit_scope(seed: u64) -> AuditScope {
    AuditScope::repository(
        OrganizationId(id("org", seed)),
        WorkspaceId(id("wsp", seed)),
        ProjectId(id("prj", seed)),
        RepositoryId(id("rep", seed)),
    )
    .expect("backup repository scope")
}

fn provenance() -> ArtifactProvenance {
    ArtifactProvenance::execution_job(
        ExecutionJobId(id("job", 3)),
        1,
        LeaseId(id("lse", 4)),
        FencingToken("42".to_owned()),
        WorkerId(id("wrk", 1)),
        WorkerInstanceId(id("wki", 2)),
        WorkerSessionId(id("wsn", 5)),
    )
    .expect("Artifact provenance")
}

fn metering_attribution() -> ArtifactMeteringAttribution {
    ArtifactMeteringAttribution {
        organization_id: OrganizationId(id("org", 1)),
        workspace_id: WorkspaceId(id("wsp", 1)),
        project_id: ProjectId(id("prj", 1)),
        repository_id: RepositoryId(id("rep", 1)),
        delivery_id: Some(DeliveryId(id("dlv", 1))),
        product_session_id: Some(ProductSessionId(id("psn", 1))),
        user_id: UserId(id("usr", 1)),
    }
}

fn chunk(
    scope: ReceiptScopeKey,
    message_seed: u64,
    artifact_id: ArtifactId,
    sequence: u64,
    bytes: Vec<u8>,
    final_chunk: bool,
) -> ArtifactChunk {
    ArtifactChunk::new(
        scope,
        ExecutionMessageId(id("xmsg", message_seed)),
        artifact_id,
        provenance(),
        1_000 + sequence,
        sequence,
        "application/octet-stream",
        Sha256Digest(sha256(&bytes)),
        bytes,
        final_chunk,
    )
}

fn adapter(
    fixture: &TlsS3Fixture,
    clock: &Arc<TestClock>,
    token: Vec<u8>,
    response_timeout: Duration,
    attempts: u8,
) -> S3ArtifactObjectStore {
    let config = S3ArtifactConfig::try_new(
        fixture.endpoint.clone(),
        BUCKET.to_owned(),
        PREFIX.to_owned(),
        S3ArtifactEncryption::try_new(
            KMS_KEY_REFERENCE.to_owned(),
            Sha256Digest(sha256(b"tenant-a-encryption-context")),
        )
        .expect("S3 encryption binding"),
        S3ArtifactTimeouts {
            connect: Duration::from_secs(2),
            response: response_timeout,
            total: Duration::from_secs(3),
        },
        S3ArtifactLimits {
            max_part_bytes: 1024 * 1024,
            max_object_bytes: 8 * 1024 * 1024,
            max_control_response_bytes: 64 * 1024,
            max_attempts: attempts,
        },
    )
    .expect("S3 config")
    .with_specific_tls_roots(vec![fixture.certificate_der.clone()])
    .expect("S3 TLS root");
    let identity: Arc<dyn S3ArtifactWorkloadIdentityPort> = Arc::new(FixtureIdentity {
        token,
        clock: Arc::clone(clock),
    });
    let runtime_clock: Arc<dyn S3ArtifactClock> = Arc::clone(clock) as Arc<dyn S3ArtifactClock>;
    S3ArtifactObjectStore::try_new(config, identity, runtime_clock).expect("S3 adapter")
}

fn assert_files_omit(root: &Path, forbidden: &[&[u8]]) {
    let mut pending = vec![root.to_path_buf()];
    while let Some(path) = pending.pop() {
        if path.is_dir() {
            pending.extend(
                fs::read_dir(path)
                    .expect("read Artifact fixture")
                    .map(|entry| entry.expect("Artifact fixture entry").path()),
            );
        } else if path.is_file() {
            let bytes = fs::read(path).expect("read Artifact fixture file");
            for value in forbidden {
                assert!(
                    !bytes.windows(value.len()).any(|window| window == *value),
                    "restricted object material reached Artifact metadata"
                );
            }
        }
    }
}

struct StaticSnapshotSource(BackupComponentKind);

impl BackupSnapshotSource for StaticSnapshotSource {
    fn kind(&self) -> BackupComponentKind {
        self.0
    }

    fn snapshot(
        &mut self,
        request: &BackupSnapshotRequest,
    ) -> Result<BackupComponentSnapshot, BackupSnapshotSourceError> {
        BackupComponentSnapshot::try_new(
            self.0,
            request.scope().clone(),
            request.consistency_cut_digest().clone(),
            Sha256Digest(sha256(format!("checkpoint:{:?}", self.0).as_bytes())),
            Sha256Digest(sha256(format!("content:{:?}", self.0).as_bytes())),
            1,
            1,
        )
        .map_err(|_| BackupSnapshotSourceError::new())
    }
}

fn capture_manifest(
    adapter: &S3ArtifactObjectStore,
    scope: &AuditScope,
) -> winwincode_backup::BackupManifest {
    let mut artifact_source = adapter.backup_source(scope.clone());
    let mut others = BackupComponentKind::REQUIRED
        .into_iter()
        .filter(|kind| *kind != BackupComponentKind::ArtifactObjects)
        .map(StaticSnapshotSource)
        .collect::<Vec<_>>();
    let mut sources = others
        .iter_mut()
        .map(|source| source as &mut dyn BackupSnapshotSource)
        .collect::<Vec<_>>();
    sources.push(&mut artifact_source);
    BackupCaptureCoordinator::capture(
        BackupId::try_new(id("bkp", 1)).expect("backup id"),
        scope.clone(),
        "cn-east-1",
        10_000,
        Sha256Digest(sha256(b"consistent-cut")),
        &mut sources,
    )
    .expect("capture S3 object manifest")
}

fn complete_across_client_restart(
    root: &Path,
    adapter: &S3ArtifactObjectStore,
    bytes: &[u8],
) -> (ArtifactId, ReceiptScopeKey, Sha256Digest) {
    let artifact_id = ArtifactId(id("art", 7));
    let scope = artifact_scope("repository:s3-primary");
    let digest = Sha256Digest(sha256(bytes));
    let split = bytes.len() / 2;
    let mut store = ArtifactStore::open(root, Box::new(adapter.clone())).expect("Artifact store");
    store
        .open_artifact(ArtifactOpen::new(
            scope.clone(),
            ExecutionMessageId(id("xmsg", 1)),
            RequestId(id("req", 1)),
            artifact_id.clone(),
            "report",
            "application/octet-stream",
            digest.clone(),
            bytes.len() as u64,
            Some("report.bin".to_owned()),
            provenance(),
            metering_attribution(),
            ArtifactRetention::UntilMillis(2_000),
            1_000,
        ))
        .expect("open S3 Artifact");
    store
        .append_chunk(&chunk(
            scope.clone(),
            2,
            artifact_id.clone(),
            1,
            bytes[..split].to_vec(),
            false,
        ))
        .expect("first S3 part with lost-response replay");
    store.close().expect("close first S3 client");
    let mut restarted =
        ArtifactStore::open(root, Box::new(adapter.clone())).expect("restart S3 client");
    restarted
        .append_chunk(&chunk(
            scope.clone(),
            3,
            artifact_id.clone(),
            2,
            bytes[split..].to_vec(),
            true,
        ))
        .expect("resume and complete S3 multipart upload");
    let access = ArtifactAccess::new(
        scope.clone(),
        artifact_id.clone(),
        digest.clone(),
        provenance(),
    );
    let object = restarted
        .read_exact(&access)
        .expect("read complete S3 Artifact");
    assert_eq!(object.bytes(), bytes);
    let range = restarted
        .read_exact_range(&access, 3, 8)
        .expect("read authorized S3 Artifact range");
    assert_eq!(range.bytes(), &bytes[3..11]);
    assert_eq!(range.range().total_size(), bytes.len() as u64);
    assert_eq!(range.range().digest(), &digest);
    restarted.close().expect("close restarted S3 client");
    (artifact_id, scope, digest)
}

#[test]
fn tls_multipart_retry_range_backup_delete_and_restart_are_one_artifact_contract() {
    let root = TestDirectory::new("lifecycle");
    let clock = Arc::new(TestClock::at(10_000));
    let state = Arc::new(Mutex::new(ServiceState::with_lost_first_mutation()));
    let fixture = TlsS3Fixture::start(&state, ServerBehavior::Service);
    let adapter = adapter(
        &fixture,
        &clock,
        WORKLOAD_TOKEN.to_vec(),
        Duration::from_secs(1),
        2,
    );
    let (artifact_id, scope, digest) =
        complete_across_client_restart(&root.0, &adapter, ARTIFACT_MARKER);
    let range = adapter
        .read_range(&digest, 3, 8)
        .expect("verified S3 range")
        .expect("range exists");
    assert_eq!(range.bytes(), &ARTIFACT_MARKER[3..11]);
    assert_eq!(range.total_size(), ARTIFACT_MARKER.len() as u64);
    let reads_before_foreign = state.lock().expect("S3 state").object_reads;
    assert_foreign_scope_is_denied(&root.0, &adapter, &artifact_id, &digest);
    assert_eq!(
        state.lock().expect("S3 state").object_reads,
        reads_before_foreign
    );
    let manifest = capture_manifest(&adapter, &audit_scope(1));
    let objects = manifest
        .components()
        .iter()
        .find(|component| component.kind() == BackupComponentKind::ArtifactObjects)
        .expect("ArtifactObjects manifest component");
    assert_eq!(objects.record_count(), 1);
    assert_eq!(objects.byte_count(), ARTIFACT_MARKER.len() as u64);
    delete_after_retention(&root.0, &adapter, artifact_id, scope, digest.clone());
    assert!(adapter.read(&digest).expect("deleted S3 lookup").is_none());
    assert_eq!(state.lock().expect("S3 state").mutations, 4);
    let public = format!(
        "{adapter:?} {}",
        String::from_utf8_lossy(&manifest.encode_canonical().expect("manifest bytes"))
    );
    for forbidden in [
        WORKLOAD_TOKEN,
        ARTIFACT_MARKER,
        BUCKET.as_bytes(),
        PREFIX.as_bytes(),
        KMS_KEY_REFERENCE.as_bytes(),
    ] {
        assert!(
            !public
                .as_bytes()
                .windows(forbidden.len())
                .any(|window| window == forbidden)
        );
    }
    assert_files_omit(&root.0, &[WORKLOAD_TOKEN, ARTIFACT_MARKER]);
    fixture.finish();
}

fn assert_foreign_scope_is_denied(
    root: &Path,
    adapter: &S3ArtifactObjectStore,
    artifact_id: &ArtifactId,
    digest: &Sha256Digest,
) {
    let store = ArtifactStore::open(root, Box::new(adapter.clone())).expect("foreign S3 client");
    let error = store
        .read_exact(&ArtifactAccess::new(
            artifact_scope("repository:s3-foreign"),
            artifact_id.clone(),
            digest.clone(),
            provenance(),
        ))
        .expect_err("foreign tenant cannot read S3 bytes");
    assert_eq!(error.kind(), ArtifactErrorKind::PermissionDenied);
    store.close().expect("close foreign S3 client");
}

fn delete_after_retention(
    root: &Path,
    adapter: &S3ArtifactObjectStore,
    artifact_id: ArtifactId,
    scope: ReceiptScopeKey,
    digest: Sha256Digest,
) {
    let access = ArtifactAccess::new(scope, artifact_id, digest, provenance());
    let mut store = ArtifactStore::open(root, Box::new(adapter.clone())).expect("delete S3 client");
    let held = store
        .delete(&access, 1_999)
        .expect_err("retention blocks early S3 deletion");
    assert_eq!(held.kind(), ArtifactErrorKind::Retained);
    store
        .delete(&access, 2_000)
        .expect("delete retained S3 object");
    store.delete(&access, 2_001).expect("replay S3 deletion");
    store.close().expect("close delete S3 client");
}

#[test]
fn exact_part_conflict_abort_timeout_and_identity_fail_closed_without_leaks() {
    let clock = Arc::new(TestClock::at(20_000));
    let state = Arc::new(Mutex::new(ServiceState::default()));
    let fixture = TlsS3Fixture::start(&state, ServerBehavior::Service);
    let mut adapter = adapter(
        &fixture,
        &clock,
        WORKLOAD_TOKEN.to_vec(),
        Duration::from_secs(1),
        1,
    );
    let artifact_id = ArtifactId(id("art", 8));
    let first = b"first multipart value";
    let first_digest = Sha256Digest(sha256(first));
    let wrong_digest = adapter
        .put_chunk(
            &artifact_id,
            1,
            &Sha256Digest(sha256(b"different bytes")),
            first,
        )
        .expect_err("part digest mismatch is rejected before HTTPS");
    assert_eq!(wrong_digest.kind(), ArtifactErrorKind::DigestMismatch);
    adapter
        .put_chunk(&artifact_id, 1, &first_digest, first)
        .expect("first multipart part");
    adapter
        .put_chunk(&artifact_id, 1, &first_digest, first)
        .expect("exact multipart replay");
    let changed = b"changed multipart value";
    let conflict = adapter
        .put_chunk(&artifact_id, 1, &Sha256Digest(sha256(changed)), changed)
        .expect_err("changed multipart replay conflicts");
    assert_eq!(conflict.kind(), ArtifactErrorKind::Conflict);
    adapter.abort_upload(&artifact_id).expect("abort multipart");
    adapter
        .abort_upload(&artifact_id)
        .expect("replay abort multipart");
    assert_eq!(state.lock().expect("S3 state").mutations, 2);
    assert_corrupt_remote_object_is_rejected(&mut adapter, &state);
    fixture.finish();
    assert_network_failures_are_bounded(&clock);
}

fn assert_corrupt_remote_object_is_rejected(
    adapter: &mut S3ArtifactObjectStore,
    state: &Arc<Mutex<ServiceState>>,
) {
    let artifact_id = ArtifactId(id("art", 9));
    let bytes = b"object corrupted after acceptance";
    let digest = Sha256Digest(sha256(bytes));
    adapter
        .put_chunk(&artifact_id, 1, &digest, bytes)
        .expect("corruption fixture part");
    adapter
        .finalize(&artifact_id, 1, &digest, bytes.len() as u64)
        .expect("corruption fixture completion");
    state
        .lock()
        .expect("S3 state")
        .objects
        .get_mut(&digest.0)
        .expect("accepted object")[0] ^= 1;
    let error = adapter
        .read(&digest)
        .expect_err("corrupt remote object is rejected");
    assert_eq!(error.kind(), ArtifactErrorKind::DigestMismatch);
}

fn assert_network_failures_are_bounded(clock: &Arc<TestClock>) {
    let delayed_state = Arc::new(Mutex::new(ServiceState::default()));
    let delayed = TlsS3Fixture::start(
        &delayed_state,
        ServerBehavior::Delay(Duration::from_millis(100)),
    );
    let timeout = adapter(
        &delayed,
        clock,
        WORKLOAD_TOKEN.to_vec(),
        Duration::from_millis(20),
        2,
    );
    let error = timeout
        .read(&Sha256Digest(sha256(b"missing")))
        .expect_err("bounded S3 timeout");
    assert_eq!(error.kind(), ArtifactErrorKind::Adapter);
    delayed.finish();
    let auth_state = Arc::new(Mutex::new(ServiceState::default()));
    let auth = TlsS3Fixture::start(&auth_state, ServerBehavior::Service);
    let unauthorized = adapter(
        &auth,
        clock,
        b"foreign-s3-workload-token".to_vec(),
        Duration::from_secs(1),
        1,
    );
    let auth_error = unauthorized
        .read(&Sha256Digest(sha256(b"missing")))
        .expect_err("wrong S3 workload identity");
    assert_eq!(auth_error.kind(), ArtifactErrorKind::PermissionDenied);
    let public = format!("{timeout:?} {unauthorized:?} {error:?} {auth_error:?}");
    assert!(!public.contains("foreign-s3-workload-token"));
    assert!(!public.contains(String::from_utf8_lossy(WORKLOAD_TOKEN).as_ref()));
    auth.finish();
}

#[test]
#[ignore = "requires an explicit external S3-compatible endpoint, workload identity file, bucket, TLS root, and KMS key reference"]
fn live_external_s3_gate_requires_explicit_files_and_performs_cleanup() {
    let endpoint = std::env::var("WINWINCODE_LIVE_S3_ENDPOINT")
        .expect("WINWINCODE_LIVE_S3_ENDPOINT is required");
    let bucket =
        std::env::var("WINWINCODE_LIVE_S3_BUCKET").expect("WINWINCODE_LIVE_S3_BUCKET is required");
    let identity_file = std::env::var("WINWINCODE_LIVE_S3_IDENTITY_FILE")
        .expect("WINWINCODE_LIVE_S3_IDENTITY_FILE is required");
    let tls_root_file = std::env::var("WINWINCODE_LIVE_S3_TLS_ROOT_DER_FILE")
        .expect("WINWINCODE_LIVE_S3_TLS_ROOT_DER_FILE is required");
    let kms_key_reference = std::env::var("WINWINCODE_LIVE_S3_KMS_KEY_REFERENCE")
        .expect("WINWINCODE_LIVE_S3_KMS_KEY_REFERENCE is required");
    let token = fs::read(identity_file).expect("read S3 workload identity file");
    let tls_root = fs::read(tls_root_file).expect("read S3 TLS root file");
    let config = S3ArtifactConfig::try_new(
        endpoint,
        bucket,
        format!("winwincode-live-{}", std::process::id()),
        S3ArtifactEncryption::try_new(
            kms_key_reference,
            Sha256Digest(sha256(b"winwincode-live-object-encryption-context")),
        )
        .expect("live S3 KMS binding"),
        S3ArtifactTimeouts {
            connect: Duration::from_secs(5),
            response: Duration::from_secs(20),
            total: Duration::from_mins(1),
        },
        S3ArtifactLimits {
            max_part_bytes: 1024 * 1024,
            max_object_bytes: 8 * 1024 * 1024,
            max_control_response_bytes: 64 * 1024,
            max_attempts: 2,
        },
    )
    .expect("live S3 config")
    .with_specific_tls_roots(vec![tls_root])
    .expect("live S3 TLS root");
    let identity: Arc<dyn S3ArtifactWorkloadIdentityPort> = Arc::new(LiveIdentity(token));
    let clock: Arc<dyn S3ArtifactClock> = Arc::new(SystemS3ArtifactClock);
    let mut adapter =
        S3ArtifactObjectStore::try_new(config, identity, clock).expect("live S3 Artifact adapter");
    let artifact_id = ArtifactId(id("art", 9_999));
    let sentinel = format!("live-s3-sentinel-{}", std::process::id()).into_bytes();
    let digest = Sha256Digest(sha256(&sentinel));
    adapter
        .put_chunk(&artifact_id, 1, &digest, &sentinel)
        .expect("live S3 multipart part");
    adapter
        .finalize(&artifact_id, 1, &digest, sentinel.len() as u64)
        .expect("live S3 multipart completion");
    assert_eq!(adapter.read(&digest).expect("live S3 read"), Some(sentinel));
    adapter.delete(&digest).expect("live S3 cleanup");
}
