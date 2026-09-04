// SPDX-License-Identifier: Apache-2.0

use std::{
    collections::BTreeMap,
    fs,
    io::{Read, Write},
    net::{TcpListener, TcpStream},
    path::PathBuf,
    sync::{
        Arc, Mutex,
        atomic::{AtomicU64, Ordering},
    },
    thread,
    time::Duration,
};

use base64::{Engine as _, engine::general_purpose::STANDARD};
use rcgen::{CertifiedKey, generate_simple_self_signed};
use rustls::{
    ServerConfig, ServerConnection, StreamOwned,
    pki_types::{PrivateKeyDer, PrivatePkcs8KeyDer},
};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use winwincode_api::generated::{
    Actor, CredentialReferenceCreateCommand, CredentialReferenceCreateCommandCommand,
    CredentialReferenceCreatePayload, CredentialReferenceRotateCommand,
    CredentialReferenceRotateCommandCommand, CredentialReferenceRotatePayload, OrganizationScope,
    OrganizationScopeKind, Scope,
};
use winwincode_control_plane::{
    CredentialReferenceResolution, CredentialReferenceService, ProductStateStorage, ResolvedSecret,
    SecretStoreError, SecretStoreErrorKind, SecretStorePort, VaultKmsClock, VaultKmsClockError,
    VaultKmsNetworkAdapter, VaultKmsNetworkConfig, VaultKmsNetworkTimeouts,
    VaultKmsWorkloadCredential, VaultKmsWorkloadIdentityPort,
};
use winwincode_domain::{
    CredentialReferenceId, OrganizationId, RequestId, Revision, SchemaVersion, UserId,
};
use winwincode_domain::{UserActor, UserActorKind};
use winwincode_storage::SqliteStorage;

const REQUEST_SCHEMA: &str = "winwincode.vault-kms-network-request.v1";
const RESPONSE_SCHEMA: &str = "winwincode.vault-kms-network-response.v1";
const WORKLOAD_TOKEN: &[u8] = b"vault-workload-token-fixture";
const INITIAL_SECRET: &[u8] = b"VAULT_NETWORK_INITIAL_SECRET";
const ROTATED_SECRET: &[u8] = b"VAULT_NETWORK_ROTATED_SECRET";
const LEASE_TTL_MS: u64 = 100;

static NEXT_DIRECTORY: AtomicU64 = AtomicU64::new(1);

#[derive(Default)]
struct TestClock(AtomicU64);

impl TestClock {
    fn at(value: u64) -> Self {
        Self(AtomicU64::new(value))
    }

    fn set(&self, value: u64) {
        self.0.store(value, Ordering::Relaxed);
    }
}

impl VaultKmsClock for TestClock {
    fn now_ms(&self) -> Result<u64, VaultKmsClockError> {
        Ok(self.0.load(Ordering::Relaxed))
    }
}

struct FixtureIdentity {
    token: Vec<u8>,
    clock: Arc<TestClock>,
    ttl_ms: u64,
}

impl VaultKmsWorkloadIdentityPort for FixtureIdentity {
    fn issue(&self) -> Result<VaultKmsWorkloadCredential, SecretStoreError> {
        VaultKmsWorkloadCredential::try_new(
            self.token.clone(),
            self.clock
                .0
                .load(Ordering::Relaxed)
                .checked_add(self.ttl_ms)
                .ok_or_else(SecretStoreError::unavailable)?,
        )
    }
}

impl Drop for FixtureIdentity {
    fn drop(&mut self) {
        self.token.fill(0);
    }
}

struct TestDirectory(PathBuf);

impl TestDirectory {
    fn new(label: &str) -> Self {
        let sequence = NEXT_DIRECTORY.fetch_add(1, Ordering::Relaxed);
        let path = std::env::temp_dir().join(format!(
            "winwincode-vault-network-{label}-{}-{sequence}",
            std::process::id()
        ));
        let _ = fs::remove_dir_all(&path);
        fs::create_dir_all(&path).expect("create Vault network fixture");
        Self(path)
    }
}

impl Drop for TestDirectory {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

fn assert_files_omit(root: &std::path::Path, forbidden: &[&[u8]]) {
    let mut pending = vec![root.to_path_buf()];
    while let Some(path) = pending.pop() {
        if path.is_dir() {
            pending.extend(
                fs::read_dir(path)
                    .expect("read Vault network fixture")
                    .map(|entry| entry.expect("Vault network entry").path()),
            );
        } else if path.is_file() {
            let bytes = fs::read(path).expect("read Vault network fixture file");
            for value in forbidden {
                assert!(
                    !bytes.windows(value.len()).any(|window| window == *value),
                    "restricted Vault material reached a durable file"
                );
            }
        }
    }
}

fn id(prefix: &str, seed: u64) -> String {
    format!("{prefix}_{seed:026}")
}

fn scope(seed: u64) -> Scope {
    Scope::OrganizationScope(OrganizationScope {
        kind: OrganizationScopeKind::Organization,
        organization_id: OrganizationId(id("org", seed)),
    })
}

fn actor(seed: u64) -> Actor {
    Actor::UserActor(UserActor {
        id: UserId(id("usr", seed)),
        kind: UserActorKind::User,
    })
}

fn create_command(seed: u64) -> CredentialReferenceCreateCommand {
    CredentialReferenceCreateCommand {
        actor: actor(seed),
        command: CredentialReferenceCreateCommandCommand::CredentialReferenceCreate,
        expected_revision: Revision(0),
        payload: CredentialReferenceCreatePayload {
            credential_reference_id: CredentialReferenceId(id("crd", seed)),
            display_name: "Network Vault/KMS credential".to_owned(),
            provider_id: "provider-vault-network".to_owned(),
            vault_locator: "vault-kms://network-write-only".to_owned(),
        },
        request_id: RequestId(id("req", seed)),
        schema_version: SchemaVersion::WinwincodeV1,
        scope: scope(seed),
    }
}

fn create_reference(
    storage: &mut SqliteStorage,
    seed: u64,
) -> (
    CredentialReferenceCreateCommand,
    CredentialReferenceResolution,
) {
    let command = create_command(seed);
    CredentialReferenceService::new(storage)
        .create(&command, 1_900_000_000_000)
        .expect("create network Vault metadata");
    let reference = CredentialReferenceService::new(storage)
        .resolve(&command.scope, &command.payload.credential_reference_id)
        .expect("resolve network Vault metadata");
    (command, reference)
}

fn rotate_metadata(
    storage: &mut SqliteStorage,
    command: &CredentialReferenceCreateCommand,
) -> CredentialReferenceResolution {
    CredentialReferenceService::new(storage)
        .rotate(
            &CredentialReferenceRotateCommand {
                actor: command.actor.clone(),
                command: CredentialReferenceRotateCommandCommand::CredentialReferenceRotate,
                expected_revision: Revision(1),
                payload: CredentialReferenceRotatePayload {
                    credential_reference_id: command.payload.credential_reference_id.clone(),
                    vault_locator: "vault-kms://network-rotated-write-only".to_owned(),
                },
                request_id: RequestId(id("req", 100)),
                schema_version: SchemaVersion::WinwincodeV1,
                scope: command.scope.clone(),
            },
            1_900_000_001_000,
        )
        .expect("rotate network Vault metadata");
    CredentialReferenceService::new(storage)
        .resolve(&command.scope, &command.payload.credential_reference_id)
        .expect("resolve rotated network Vault metadata")
}

#[derive(Deserialize)]
#[serde(tag = "operation", rename_all = "camelCase")]
enum FixtureRequest {
    Resolve {
        schema: String,
        operation_id: String,
        reference: Value,
    },
    Write {
        schema: String,
        operation_id: String,
        reference: Value,
        rotation_version: u64,
        secret: String,
    },
    Renew {
        schema: String,
        operation_id: String,
        reference: Value,
        lease_id: String,
    },
    Revoke {
        schema: String,
        operation_id: String,
        reference: Value,
    },
    RotateKey {
        schema: String,
        operation_id: String,
        key_version: u64,
    },
}

#[derive(Serialize)]
#[serde(tag = "result", rename_all = "camelCase")]
enum FixtureResponse {
    Lease {
        schema: &'static str,
        operation_id: String,
        rotation_version: u64,
        lease_id: String,
        issued_at_ms: u64,
        expires_at_ms: u64,
        secret: String,
    },
    Written {
        schema: &'static str,
        operation_id: String,
        rotation_version: u64,
        key_version: u64,
        replayed: bool,
    },
    Renewed {
        schema: &'static str,
        operation_id: String,
        rotation_version: u64,
        lease_id: String,
        issued_at_ms: u64,
        expires_at_ms: u64,
    },
    Revoked {
        schema: &'static str,
        operation_id: String,
        removed_versions: u64,
    },
    KeyRotated {
        schema: &'static str,
        operation_id: String,
        key_version: u64,
        rewrapped_versions: u64,
    },
}

struct VersionValue {
    secret: Vec<u8>,
    key_version: u64,
}

struct LeaseValue {
    rotation_version: u64,
    expires_at_ms: u64,
}

struct ServiceState {
    clock: Arc<TestClock>,
    versions: BTreeMap<u64, VersionValue>,
    leases: BTreeMap<String, LeaseValue>,
    operation_responses: BTreeMap<String, Vec<u8>>,
    active_key_version: u64,
    next_lease: u64,
    mutations: u64,
    revoked: bool,
    drop_first_success: bool,
}

impl ServiceState {
    fn new(clock: Arc<TestClock>, drop_first_success: bool) -> Self {
        Self {
            clock,
            versions: BTreeMap::new(),
            leases: BTreeMap::new(),
            operation_responses: BTreeMap::new(),
            active_key_version: 1,
            next_lease: 1,
            mutations: 0,
            revoked: false,
            drop_first_success,
        }
    }

    fn apply(&mut self, request: FixtureRequest) -> ServiceReply {
        let operation_id = fixture_operation_id(&request);
        if let Some(response) = self.operation_responses.get(&operation_id) {
            return ServiceReply::json(200, response.clone());
        }
        if fixture_schema(&request) != REQUEST_SCHEMA {
            return ServiceReply::empty(422);
        }
        match request {
            FixtureRequest::Write {
                operation_id,
                reference,
                rotation_version,
                secret,
                ..
            } => self.write(operation_id, &reference, rotation_version, secret),
            FixtureRequest::Resolve {
                operation_id,
                reference,
                ..
            } => self.resolve(operation_id, &reference),
            FixtureRequest::Renew {
                operation_id,
                reference,
                lease_id,
                ..
            } => self.renew(operation_id, &reference, &lease_id),
            FixtureRequest::Revoke {
                operation_id,
                reference,
                ..
            } => self.revoke(operation_id, &reference),
            FixtureRequest::RotateKey {
                operation_id,
                key_version,
                ..
            } => self.rotate_key(operation_id, key_version),
        }
    }

    fn write(
        &mut self,
        operation_id: String,
        reference: &Value,
        rotation_version: u64,
        secret: String,
    ) -> ServiceReply {
        if reference_rotation(reference).is_none() || self.revoked {
            return ServiceReply::empty(410);
        }
        let mut encoded = secret.into_bytes();
        let decoded = STANDARD.decode(&encoded);
        encoded.fill(0);
        let Ok(mut decoded) = decoded else {
            return ServiceReply::empty(422);
        };
        let existing = self
            .versions
            .get(&rotation_version)
            .map(|value| (value.secret == decoded, value.key_version));
        if let Some((exact, key_version)) = existing {
            decoded.fill(0);
            return if exact {
                let response = FixtureResponse::Written {
                    schema: RESPONSE_SCHEMA,
                    operation_id: operation_id.clone(),
                    rotation_version,
                    key_version,
                    replayed: true,
                };
                self.success(operation_id, &response, false)
            } else {
                ServiceReply::empty(409)
            };
        }
        self.versions.insert(
            rotation_version,
            VersionValue {
                secret: decoded,
                key_version: self.active_key_version,
            },
        );
        let response = FixtureResponse::Written {
            schema: RESPONSE_SCHEMA,
            operation_id: operation_id.clone(),
            rotation_version,
            key_version: self.active_key_version,
            replayed: false,
        };
        self.success(operation_id, &response, true)
    }

    fn resolve(&mut self, operation_id: String, reference: &Value) -> ServiceReply {
        let Some(rotation_version) = reference_rotation(reference) else {
            return ServiceReply::empty(422);
        };
        if self.revoked {
            return ServiceReply::empty(410);
        }
        let Some(secret) = self
            .versions
            .get(&rotation_version)
            .map(|value| STANDARD.encode(&value.secret))
        else {
            return ServiceReply::empty(404);
        };
        let (lease_id, issued_at_ms, expires_at_ms) = self.issue_lease(rotation_version);
        let response = FixtureResponse::Lease {
            schema: RESPONSE_SCHEMA,
            operation_id: operation_id.clone(),
            rotation_version,
            lease_id,
            issued_at_ms,
            expires_at_ms,
            secret,
        };
        self.success(operation_id, &response, false)
    }

    fn renew(&mut self, operation_id: String, reference: &Value, lease_id: &str) -> ServiceReply {
        let Some(rotation_version) = reference_rotation(reference) else {
            return ServiceReply::empty(422);
        };
        let current_time = self.clock.0.load(Ordering::Relaxed);
        let renewable = self.leases.get(lease_id).is_some_and(|lease| {
            lease.rotation_version == rotation_version
                && current_time < lease.expires_at_ms
                && !self.revoked
        });
        if !renewable {
            return ServiceReply::empty(410);
        }
        let (lease_id, issued_at_ms, expires_at_ms) = self.issue_lease(rotation_version);
        let response = FixtureResponse::Renewed {
            schema: RESPONSE_SCHEMA,
            operation_id: operation_id.clone(),
            rotation_version,
            lease_id,
            issued_at_ms,
            expires_at_ms,
        };
        self.success(operation_id, &response, false)
    }

    fn revoke(&mut self, operation_id: String, reference: &Value) -> ServiceReply {
        if reference_rotation(reference).is_none() {
            return ServiceReply::empty(422);
        }
        let removed_versions = u64::try_from(self.versions.len()).unwrap_or(u64::MAX);
        for value in self.versions.values_mut() {
            value.secret.fill(0);
        }
        self.versions.clear();
        self.revoked = true;
        let response = FixtureResponse::Revoked {
            schema: RESPONSE_SCHEMA,
            operation_id: operation_id.clone(),
            removed_versions,
        };
        self.success(operation_id, &response, true)
    }

    fn rotate_key(&mut self, operation_id: String, key_version: u64) -> ServiceReply {
        if key_version <= self.active_key_version {
            return ServiceReply::empty(409);
        }
        let rewrapped_versions = u64::try_from(self.versions.len()).unwrap_or(u64::MAX);
        for value in self.versions.values_mut() {
            value.key_version = key_version;
        }
        self.active_key_version = key_version;
        let response = FixtureResponse::KeyRotated {
            schema: RESPONSE_SCHEMA,
            operation_id: operation_id.clone(),
            key_version,
            rewrapped_versions,
        };
        self.success(operation_id, &response, true)
    }

    fn issue_lease(&mut self, rotation_version: u64) -> (String, u64, u64) {
        let issued_at_ms = self.clock.0.load(Ordering::Relaxed);
        let expires_at_ms = issued_at_ms + LEASE_TTL_MS;
        let lease_id = format!("vkl_{:026}", self.next_lease);
        self.next_lease += 1;
        self.leases.insert(
            lease_id.clone(),
            LeaseValue {
                rotation_version,
                expires_at_ms,
            },
        );
        (lease_id, issued_at_ms, expires_at_ms)
    }

    fn success(
        &mut self,
        operation_id: String,
        response: &FixtureResponse,
        mutation: bool,
    ) -> ServiceReply {
        let bytes = serde_json::to_vec(response).expect("serialize fixture response");
        self.operation_responses.insert(operation_id, bytes.clone());
        if mutation {
            self.mutations += 1;
        }
        if self.drop_first_success {
            self.drop_first_success = false;
            ServiceReply::drop_connection()
        } else {
            ServiceReply::json(200, bytes)
        }
    }
}

impl Drop for ServiceState {
    fn drop(&mut self) {
        for value in self.versions.values_mut() {
            value.secret.fill(0);
        }
        for response in self.operation_responses.values_mut() {
            response.fill(0);
        }
    }
}

struct ServiceReply {
    status: u16,
    body: Vec<u8>,
    drop_connection: bool,
}

impl ServiceReply {
    fn json(status: u16, body: Vec<u8>) -> Self {
        Self {
            status,
            body,
            drop_connection: false,
        }
    }

    fn empty(status: u16) -> Self {
        Self::json(status, Vec::new())
    }

    fn drop_connection() -> Self {
        Self {
            status: 0,
            body: Vec::new(),
            drop_connection: true,
        }
    }
}

#[derive(Clone, Copy)]
enum ServerBehavior {
    Service,
    Delay(Duration),
    NonCanonical,
}

struct TlsVaultFixture {
    endpoint: String,
    certificate_der: Vec<u8>,
    server: thread::JoinHandle<()>,
}

impl TlsVaultFixture {
    fn start(state: &Arc<Mutex<ServiceState>>, behavior: ServerBehavior, requests: usize) -> Self {
        let _ = rustls::crypto::aws_lc_rs::default_provider().install_default();
        let CertifiedKey { cert, signing_key } =
            generate_simple_self_signed(vec!["localhost".to_owned()])
                .expect("generate Vault fixture certificate");
        let private_key =
            PrivateKeyDer::Pkcs8(PrivatePkcs8KeyDer::from(signing_key.serialize_der()));
        let config = ServerConfig::builder()
            .with_no_client_auth()
            .with_single_cert(vec![cert.der().clone()], private_key)
            .expect("build Vault TLS fixture");
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind Vault TLS fixture");
        let address = listener.local_addr().expect("Vault fixture address");
        let server_state = Arc::clone(state);
        let server = thread::spawn(move || {
            let config = Arc::new(config);
            for _ in 0..requests {
                let (socket, _) = listener.accept().expect("accept Vault TLS request");
                let connection =
                    ServerConnection::new(Arc::clone(&config)).expect("Vault TLS connection");
                let mut stream = StreamOwned::new(connection, socket);
                let mut request = read_http_request(&mut stream);
                let reply = match behavior {
                    ServerBehavior::Service => handle_service_request(&request, &server_state),
                    ServerBehavior::Delay(delay) => {
                        thread::sleep(delay);
                        ServiceReply::json(200, b"{}".to_vec())
                    }
                    ServerBehavior::NonCanonical => ServiceReply::json(
                        200,
                        b"{\"result\":\"revoked\",\"schema\":\"winwincode.vault-kms-network-response.v1\",\"operationId\":\"wrong\",\"removedVersions\":0} ".to_vec(),
                    ),
                };
                request.fill(0);
                if !reply.drop_connection {
                    write_http_response(&mut stream, &reply);
                }
            }
        });
        Self {
            endpoint: format!("https://localhost:{}/v1/vault/operations", address.port()),
            certificate_der: cert.der().to_vec(),
            server,
        }
    }

    fn finish(self) {
        self.server.join().expect("join Vault TLS fixture");
    }
}

fn handle_service_request(request: &[u8], state: &Arc<Mutex<ServiceState>>) -> ServiceReply {
    let Some(header_end) = find_bytes(request, b"\r\n\r\n") else {
        return ServiceReply::empty(400);
    };
    if !header_has_exact_bearer(&request[..header_end], WORKLOAD_TOKEN) {
        return ServiceReply::empty(401);
    }
    let body = &request[header_end + 4..];
    let Ok(request) = serde_json::from_slice::<FixtureRequest>(body) else {
        return ServiceReply::empty(422);
    };
    state.lock().expect("Vault service lock").apply(request)
}

fn read_http_request(stream: &mut StreamOwned<ServerConnection, TcpStream>) -> Vec<u8> {
    let mut request = Vec::new();
    let mut buffer = [0_u8; 4 * 1024];
    loop {
        let count = stream.read(&mut buffer).expect("read Vault TLS request");
        if count == 0 {
            return request;
        }
        request.extend_from_slice(&buffer[..count]);
        let Some(header_end) = find_bytes(&request, b"\r\n\r\n") else {
            continue;
        };
        let length = content_length(&request[..header_end]);
        if request.len() >= header_end + 4 + length {
            return request;
        }
    }
}

fn write_http_response(
    stream: &mut StreamOwned<ServerConnection, TcpStream>,
    reply: &ServiceReply,
) {
    let reason = match reply.status {
        200 => "OK",
        401 => "Unauthorized",
        404 => "Not Found",
        409 => "Conflict",
        410 => "Gone",
        _ => "Unprocessable Entity",
    };
    let header = format!(
        "HTTP/1.1 {} {}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
        reply.status,
        reason,
        reply.body.len()
    );
    let _ = stream.write_all(header.as_bytes());
    let _ = stream.write_all(&reply.body);
    let _ = stream.flush();
}

fn find_bytes(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    haystack
        .windows(needle.len())
        .position(|window| window == needle)
}

fn content_length(headers: &[u8]) -> usize {
    std::str::from_utf8(headers)
        .expect("UTF-8 request headers")
        .lines()
        .find_map(|line| {
            line.strip_prefix("Content-Length: ")
                .or_else(|| line.strip_prefix("content-length: "))
        })
        .expect("Content-Length header")
        .parse()
        .expect("numeric Content-Length")
}

fn header_has_exact_bearer(headers: &[u8], token: &[u8]) -> bool {
    headers
        .split(|byte| *byte == b'\n')
        .map(|line| line.strip_suffix(b"\r").unwrap_or(line))
        .any(|line| {
            let Some(colon) = line.iter().position(|byte| *byte == b':') else {
                return false;
            };
            if !line[..colon].eq_ignore_ascii_case(b"authorization") {
                return false;
            }
            let value = line[colon + 1..]
                .strip_prefix(b" ")
                .unwrap_or(&line[colon + 1..]);
            value
                .strip_prefix(b"Bearer ")
                .is_some_and(|candidate| candidate == token)
        })
}

fn fixture_operation_id(request: &FixtureRequest) -> String {
    match request {
        FixtureRequest::Resolve { operation_id, .. }
        | FixtureRequest::Write { operation_id, .. }
        | FixtureRequest::Renew { operation_id, .. }
        | FixtureRequest::Revoke { operation_id, .. }
        | FixtureRequest::RotateKey { operation_id, .. } => operation_id.clone(),
    }
}

fn fixture_schema(request: &FixtureRequest) -> &str {
    match request {
        FixtureRequest::Resolve { schema, .. }
        | FixtureRequest::Write { schema, .. }
        | FixtureRequest::Renew { schema, .. }
        | FixtureRequest::Revoke { schema, .. }
        | FixtureRequest::RotateKey { schema, .. } => schema,
    }
}

fn reference_rotation(reference: &Value) -> Option<u64> {
    reference.get("rotationVersion")?.as_u64()
}

fn network_adapter(
    fixture: &TlsVaultFixture,
    clock: &Arc<TestClock>,
    token: Vec<u8>,
    response_timeout: Duration,
    attempts: u8,
) -> VaultKmsNetworkAdapter {
    let config = VaultKmsNetworkConfig::try_new(
        fixture.endpoint.clone(),
        VaultKmsNetworkTimeouts {
            connect: Duration::from_secs(2),
            response: response_timeout,
            total: Duration::from_secs(3),
        },
        64 * 1024,
        attempts,
    )
    .expect("Vault network config")
    .with_specific_tls_roots(vec![fixture.certificate_der.clone()])
    .expect("Vault TLS root");
    let identity: Arc<dyn VaultKmsWorkloadIdentityPort> = Arc::new(FixtureIdentity {
        token,
        clock: Arc::clone(clock),
        ttl_ms: 1_000,
    });
    let clock: Arc<dyn VaultKmsClock> = Arc::clone(clock) as Arc<dyn VaultKmsClock>;
    VaultKmsNetworkAdapter::try_new(config, identity, clock).expect("Vault network adapter")
}

#[test]
fn tls_workload_identity_retry_rotation_lease_and_revocation_are_one_network_contract() {
    let root = TestDirectory::new("lifecycle");
    let mut storage = SqliteStorage::open(&root.0).expect("open Credential metadata");
    let (command, version_one) = create_reference(&mut storage, 1);
    let clock = Arc::new(TestClock::at(10_000));
    let state = Arc::new(Mutex::new(ServiceState::new(Arc::clone(&clock), true)));
    let fixture = TlsVaultFixture::start(&state, ServerBehavior::Service, 11);
    let adapter = network_adapter(
        &fixture,
        &clock,
        WORKLOAD_TOKEN.to_vec(),
        Duration::from_secs(1),
        2,
    );

    let write = adapter
        .store(
            &version_one,
            ResolvedSecret::from_bytes(INITIAL_SECRET.to_vec()).expect("initial secret"),
        )
        .expect("retry exact write after lost TLS response");
    assert!(!write.replayed());
    let first = adapter
        .resolve_lease(&version_one)
        .expect("leased secret one");
    assert_eq!(first.receipt().issued_at_ms(), 10_000);
    assert_eq!(first.receipt().expires_at_ms(), 10_100);
    let accepted_one = first.into_secret();

    let rotated_write = adapter
        .rotate_secret(
            &version_one,
            ResolvedSecret::from_bytes(ROTATED_SECRET.to_vec()).expect("rotated secret"),
        )
        .expect("stage remote rotation");
    assert_eq!(rotated_write.rotation_version(), 2);
    let key_rotation = adapter
        .rotate_customer_key(2)
        .expect("rotate remote customer key");
    assert_eq!(key_rotation.key_version(), 2);
    assert_eq!(key_rotation.rewrapped_versions(), 2);
    assert_eq!(
        adapter
            .resolve(&version_one)
            .expect("old version after CMK rotation")
            .expose(),
        INITIAL_SECRET
    );

    let version_two = rotate_metadata(&mut storage, &command);
    let second = adapter
        .resolve_lease(&version_two)
        .expect("leased secret two");
    let first_lease = second.receipt().clone();
    let accepted_two = second.into_secret();
    clock.set(10_050);
    let renewed = adapter
        .renew_lease(&version_two, &first_lease)
        .expect("renew unexpired lease");
    assert_eq!(renewed.issued_at_ms(), 10_050);
    clock.set(first_lease.expires_at_ms());
    assert_eq!(
        adapter
            .renew_lease(&version_two, &first_lease)
            .expect_err("expired lease is not renewable")
            .kind(),
        SecretStoreErrorKind::Missing
    );
    assert_eq!(
        adapter
            .revoke(&version_two)
            .expect("remote revoke")
            .removed_versions(),
        2
    );
    assert_eq!(
        adapter
            .resolve(&version_two)
            .expect_err("new lookup observes revocation")
            .kind(),
        SecretStoreErrorKind::Missing
    );
    assert_eq!(accepted_one.expose(), INITIAL_SECRET);
    assert_eq!(accepted_two.expose(), ROTATED_SECRET);
    assert_eq!(state.lock().expect("state").mutations, 4);
    let public = format!("{adapter:?} {write:?} {rotated_write:?} {key_rotation:?}");
    assert!(!public.contains(String::from_utf8_lossy(WORKLOAD_TOKEN).as_ref()));
    assert!(!public.contains(String::from_utf8_lossy(INITIAL_SECRET).as_ref()));
    assert!(!public.contains(String::from_utf8_lossy(ROTATED_SECRET).as_ref()));
    assert_files_omit(&root.0, &[WORKLOAD_TOKEN, INITIAL_SECRET, ROTATED_SECRET]);

    fixture.finish();
    Box::new(storage)
        .close()
        .expect("close Credential metadata");
}

#[test]
fn tls_timeout_wrong_identity_and_noncanonical_response_fail_closed() {
    let root = TestDirectory::new("fail-closed");
    let mut storage = SqliteStorage::open(&root.0).expect("open Credential metadata");
    let (_, reference) = create_reference(&mut storage, 2);
    let clock = Arc::new(TestClock::at(20_000));

    let delayed_state = Arc::new(Mutex::new(ServiceState::new(Arc::clone(&clock), false)));
    let delayed = TlsVaultFixture::start(
        &delayed_state,
        ServerBehavior::Delay(Duration::from_millis(100)),
        2,
    );
    let timeout_adapter = network_adapter(
        &delayed,
        &clock,
        WORKLOAD_TOKEN.to_vec(),
        Duration::from_millis(20),
        2,
    );
    assert_eq!(
        timeout_adapter
            .resolve(&reference)
            .expect_err("bounded Vault timeout")
            .kind(),
        SecretStoreErrorKind::Unavailable
    );
    delayed.finish();

    let unauthorized_state = Arc::new(Mutex::new(ServiceState::new(Arc::clone(&clock), false)));
    let unauthorized = TlsVaultFixture::start(&unauthorized_state, ServerBehavior::Service, 1);
    let unauthorized_adapter = network_adapter(
        &unauthorized,
        &clock,
        b"foreign-workload-token".to_vec(),
        Duration::from_secs(1),
        1,
    );
    let auth_error = unauthorized_adapter
        .resolve(&reference)
        .expect_err("wrong workload identity");
    assert_eq!(auth_error.kind(), SecretStoreErrorKind::Unavailable);
    unauthorized.finish();

    let corrupt_state = Arc::new(Mutex::new(ServiceState::new(Arc::clone(&clock), false)));
    let corrupt = TlsVaultFixture::start(&corrupt_state, ServerBehavior::NonCanonical, 1);
    let corrupt_adapter = network_adapter(
        &corrupt,
        &clock,
        WORKLOAD_TOKEN.to_vec(),
        Duration::from_secs(1),
        1,
    );
    let protocol_error = corrupt_adapter
        .revoke(&reference)
        .expect_err("noncanonical remote response");
    assert_eq!(protocol_error.kind(), SecretStoreErrorKind::Corrupt);
    let public = format!(
        "{timeout_adapter:?} {unauthorized_adapter:?} {corrupt_adapter:?} {auth_error:?} {protocol_error:?}"
    );
    assert!(!public.contains(String::from_utf8_lossy(WORKLOAD_TOKEN).as_ref()));
    assert!(!public.contains("foreign-workload-token"));
    assert_files_omit(
        &root.0,
        &[WORKLOAD_TOKEN, b"foreign-workload-token", INITIAL_SECRET],
    );
    corrupt.finish();
    Box::new(storage)
        .close()
        .expect("close Credential metadata");
}

#[test]
#[ignore = "requires an explicit external Vault/KMS endpoint, workload identity file, and TLS root"]
fn live_external_vault_kms_gate_requires_explicit_endpoint_and_workload_identity_files() {
    let endpoint = std::env::var("WINWINCODE_LIVE_VAULT_KMS_ENDPOINT")
        .expect("WINWINCODE_LIVE_VAULT_KMS_ENDPOINT is required");
    let identity_file = std::env::var("WINWINCODE_LIVE_VAULT_KMS_IDENTITY_FILE")
        .expect("WINWINCODE_LIVE_VAULT_KMS_IDENTITY_FILE is required");
    let tls_root_file = std::env::var("WINWINCODE_LIVE_VAULT_KMS_TLS_ROOT_DER_FILE")
        .expect("WINWINCODE_LIVE_VAULT_KMS_TLS_ROOT_DER_FILE is required");
    let token = fs::read(identity_file).expect("read workload identity file");
    let tls_root = fs::read(tls_root_file).expect("read Vault/KMS TLS root");
    let now_ms = u64::try_from(
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("wall clock")
            .as_millis(),
    )
    .expect("wall clock milliseconds");
    let clock = Arc::new(TestClock::at(now_ms));
    let config = VaultKmsNetworkConfig::try_new(
        endpoint,
        VaultKmsNetworkTimeouts {
            connect: Duration::from_secs(5),
            response: Duration::from_secs(10),
            total: Duration::from_secs(30),
        },
        64 * 1024,
        2,
    )
    .expect("live Vault/KMS config")
    .with_specific_tls_roots(vec![tls_root])
    .expect("live Vault/KMS root");
    let identity: Arc<dyn VaultKmsWorkloadIdentityPort> = Arc::new(FixtureIdentity {
        token,
        clock: Arc::clone(&clock),
        ttl_ms: 5 * 60 * 1000,
    });
    let runtime_clock: Arc<dyn VaultKmsClock> = Arc::clone(&clock) as Arc<dyn VaultKmsClock>;
    let adapter = VaultKmsNetworkAdapter::try_new(config, identity, runtime_clock)
        .expect("live Vault/KMS adapter");
    let root = TestDirectory::new("live");
    let mut storage = SqliteStorage::open(&root.0).expect("open live metadata");
    let (_, reference) = create_reference(&mut storage, 9_999);
    let mut expected = vec![0_u8; 32];
    getrandom::fill(&mut expected).expect("live secret entropy");
    adapter
        .store(
            &reference,
            ResolvedSecret::from_bytes(expected.clone()).expect("live sentinel"),
        )
        .expect("live encrypted store");
    let resolved = adapter.resolve(&reference).expect("live resolve");
    assert_eq!(resolved.expose(), expected);
    adapter.revoke(&reference).expect("live cleanup revoke");
    expected.fill(0);
    Box::new(storage).close().expect("close live metadata");
}
