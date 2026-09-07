// SPDX-License-Identifier: Apache-2.0

use std::fs;
use std::fs::OpenOptions;
use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Duration;

use base64::{Engine as _, engine::general_purpose::STANDARD};
use rcgen::{CertifiedKey, generate_simple_self_signed};
use rustls::pki_types::{PrivateKeyDer, PrivatePkcs8KeyDer};
use rustls::{ServerConfig, ServerConnection, StreamOwned};
use serde_json::{Value, json};
use winwincode_api::generated::{
    Actor, CredentialReferenceCreateCommand, CredentialReferenceCreateCommandCommand,
    CredentialReferenceCreatePayload, CredentialReferenceRevokeCommand,
    CredentialReferenceRevokeCommandCommand, CredentialReferenceRevokePayload, OrganizationScope,
    OrganizationScopeKind, Scope,
};
use winwincode_control_plane::{
    CredentialReferenceService, EnterpriseIdentityProductionVerifiers,
    EnterpriseIdentityVerifierConfig, EnterpriseIdentityVerifierTimeouts, LocalSecretStoreAdapter,
    OidcTokenVerifier, ProtocolVerificationErrorKind, ResolvedSecret, SamlResponseVerifier,
    ScimBearerVerifier, SecretStoreError, SecretStorePort,
};
use winwincode_domain::{
    CredentialReferenceId, OrganizationId, RequestId, Revision, SchemaVersion, UserId,
};
use winwincode_domain::{UserActor, UserActorKind};
use winwincode_storage::SqliteStorage;

const AUTHORITY_CREDENTIAL: &[u8] = b"fixture-verification-authority-credential";
const OIDC_TOKEN: &[u8] = b"fixture.oidc.id-token";
const SAML_RESPONSE: &[u8] = b"<Response>fixture-signed-saml</Response>";
const SCIM_BEARER: &[u8] = b"fixture-scim-bearer";
const RESPONSE_SCHEMA: &str = "winwincode.enterprise-identity-verification-response.v1";
static NEXT_FIXTURE: AtomicU64 = AtomicU64::new(1);

struct TestDirectory(PathBuf);

impl TestDirectory {
    fn new(label: &str) -> Self {
        let serial = NEXT_FIXTURE.fetch_add(1, Ordering::SeqCst);
        let path = std::env::temp_dir().join(format!(
            "winwincode-enterprise-identity-verifier-{label}-{}-{serial}",
            std::process::id()
        ));
        let _ = fs::remove_dir_all(&path);
        fs::create_dir_all(&path).expect("create verifier fixture");
        Self(path)
    }
}

impl Drop for TestDirectory {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

#[derive(Default)]
struct SecretFacts {
    resolutions: AtomicU64,
}

impl SecretStorePort for SecretFacts {
    fn resolve(
        &self,
        _reference: &winwincode_control_plane::CredentialReferenceResolution,
    ) -> Result<ResolvedSecret, SecretStoreError> {
        self.resolutions.fetch_add(1, Ordering::SeqCst);
        ResolvedSecret::from_bytes(AUTHORITY_CREDENTIAL.to_vec())
    }
}

#[derive(Clone, Copy)]
enum ReplyBehavior {
    Verified,
    SignatureRejected,
    NonCanonical,
}

struct TlsVerifierFixture {
    endpoint: String,
    certificate_der: Vec<u8>,
    observations: Arc<Mutex<Vec<RequestObservation>>>,
    server: thread::JoinHandle<()>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct RequestObservation {
    path: String,
    operation: String,
    credential_matches: bool,
    authority_matches: bool,
    url_contains_credential: bool,
}

impl TlsVerifierFixture {
    fn start(behavior: ReplyBehavior, requests: usize) -> Self {
        let _ = rustls::crypto::aws_lc_rs::default_provider().install_default();
        let CertifiedKey { cert, signing_key } =
            generate_simple_self_signed(vec!["localhost".to_owned()])
                .expect("generate verifier certificate");
        let private_key =
            PrivateKeyDer::Pkcs8(PrivatePkcs8KeyDer::from(signing_key.serialize_der()));
        let config = ServerConfig::builder()
            .with_no_client_auth()
            .with_single_cert(vec![cert.der().clone()], private_key)
            .expect("build verifier TLS fixture");
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind verifier fixture");
        let address = listener.local_addr().expect("verifier fixture address");
        let observations = Arc::new(Mutex::new(Vec::new()));
        let server_observations = Arc::clone(&observations);
        let server = thread::spawn(move || {
            let config = Arc::new(config);
            for _ in 0..requests {
                let (socket, _) = listener.accept().expect("accept verifier request");
                let connection =
                    ServerConnection::new(Arc::clone(&config)).expect("verifier TLS connection");
                let mut stream = StreamOwned::new(connection, socket);
                let Some(mut request) = read_http_request(&mut stream) else {
                    continue;
                };
                let reply = fixture_reply(&request, behavior, &server_observations);
                request.fill(0);
                write_http_response(&mut stream, reply.0, &reply.1);
            }
        });
        Self {
            endpoint: format!("https://localhost:{}/v1/identity/verify", address.port()),
            certificate_der: cert.der().to_vec(),
            observations,
            server,
        }
    }

    fn finish(self) -> Vec<RequestObservation> {
        self.server.join().expect("join verifier fixture");
        self.observations
            .lock()
            .expect("verifier observations")
            .clone()
    }
}

fn fixture_reply(
    request: &[u8],
    behavior: ReplyBehavior,
    observations: &Mutex<Vec<RequestObservation>>,
) -> (u16, Vec<u8>) {
    let Some(header_end) = find_bytes(request, b"\r\n\r\n") else {
        return (422, Vec::new());
    };
    let request_line = request
        .split(|byte| *byte == b'\n')
        .next()
        .and_then(|line| std::str::from_utf8(line).ok())
        .unwrap_or_default();
    let path = request_line
        .split_ascii_whitespace()
        .nth(1)
        .unwrap_or_default()
        .to_owned();
    let authority_matches = header_has_exact_bearer(&request[..header_end], AUTHORITY_CREDENTIAL);
    let body = &request[header_end + 4..];
    let parsed: Value = match serde_json::from_slice(body) {
        Ok(value) => value,
        Err(_) => return (422, Vec::new()),
    };
    let operation = parsed
        .get("operation")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_owned();
    let expected = match operation.as_str() {
        "verify_oidc" => OIDC_TOKEN,
        "verify_saml" => SAML_RESPONSE,
        "verify_scim" => SCIM_BEARER,
        _ => b"" as &[u8],
    };
    let credential_matches = parsed
        .get("credentialBase64")
        .and_then(Value::as_str)
        .and_then(|value| STANDARD.decode(value).ok())
        .is_some_and(|value| value == expected);
    let url_contains_credential = [OIDC_TOKEN, SAML_RESPONSE, SCIM_BEARER, AUTHORITY_CREDENTIAL]
        .iter()
        .any(|credential| {
            request_line
                .as_bytes()
                .windows(credential.len())
                .any(|window| window == *credential)
        });
    observations
        .lock()
        .expect("record verifier request")
        .push(RequestObservation {
            path,
            operation: operation.clone(),
            credential_matches,
            authority_matches,
            url_contains_credential,
        });
    match behavior {
        ReplyBehavior::SignatureRejected => (409, Vec::new()),
        ReplyBehavior::NonCanonical => (200, b"{\"schema\":\"wrong\"}".to_vec()),
        ReplyBehavior::Verified => {
            verified_response(&operation).map_or_else(|| (422, Vec::new()), |body| (200, body))
        }
    }
}

fn verified_response(operation: &str) -> Option<Vec<u8>> {
    let claims = match operation {
        "verify_oidc" => json!({
            "issuer": "https://idp.example/oidc",
            "audiences": ["winwincode"],
            "subject": "subject-1",
            "tokenId": "oidc-token-1",
            "issuedAtMillis": 1_700_000_000_000_u64,
            "notBeforeMillis": 1_700_000_000_000_u64,
            "expiresAtMillis": 1_700_000_060_000_u64
        }),
        "verify_saml" => json!({
            "issuer": "https://idp.example/saml",
            "audiences": ["winwincode"],
            "subject": "subject-1",
            "assertionId": "saml-assertion-1",
            "issuedAtMillis": 1_700_000_000_000_u64,
            "notBeforeMillis": 1_700_000_000_000_u64,
            "expiresAtMillis": 1_700_000_060_000_u64
        }),
        "verify_scim" => json!({
            "issuer": "https://idp.example/scim",
            "audiences": ["winwincode-scim"],
            "clientId": "scim-client-1",
            "expiresAtMillis": 1_700_000_060_000_u64
        }),
        _ => return None,
    };
    serde_json::to_vec(&json!({
        "schema": RESPONSE_SCHEMA,
        "operation": operation,
        "claims": claims
    }))
    .ok()
}

fn read_http_request(stream: &mut StreamOwned<ServerConnection, TcpStream>) -> Option<Vec<u8>> {
    let mut request = Vec::new();
    let mut buffer = [0_u8; 4 * 1024];
    loop {
        let count = stream.read(&mut buffer).ok()?;
        if count == 0 {
            return Some(request);
        }
        request.extend_from_slice(&buffer[..count]);
        let Some(header_end) = find_bytes(&request, b"\r\n\r\n") else {
            continue;
        };
        let length = content_length(&request[..header_end]);
        if request.len() >= header_end + 4 + length {
            return Some(request);
        }
    }
}

fn write_http_response(
    stream: &mut StreamOwned<ServerConnection, TcpStream>,
    status: u16,
    body: &[u8],
) {
    let reason = match status {
        200 => "OK",
        409 => "Conflict",
        _ => "Unprocessable Entity",
    };
    let content_type = if status == 200 {
        "Content-Type: application/json\r\n"
    } else {
        ""
    };
    let header = format!(
        "HTTP/1.1 {status} {reason}\r\n{content_type}Content-Length: {}\r\nConnection: close\r\n\r\n",
        body.len()
    );
    stream
        .write_all(header.as_bytes())
        .expect("write verifier response header");
    stream
        .write_all(body)
        .expect("write verifier response body");
    stream.flush().expect("flush verifier response");
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
            display_name: "Enterprise identity verifier authority".to_owned(),
            provider_id: "enterprise-identity-verifier".to_owned(),
            vault_locator: "secret-store://write-only".to_owned(),
        },
        request_id: RequestId(id("req", seed)),
        schema_version: SchemaVersion::WinwincodeV1,
        scope: scope(seed),
    }
}

fn verifier_set(
    root: &Path,
    fixture: &TlsVerifierFixture,
    secrets: Arc<SecretFacts>,
    seed: u64,
) -> EnterpriseIdentityProductionVerifiers {
    verifier_set_with_root(
        root,
        fixture,
        fixture.certificate_der.clone(),
        secrets,
        seed,
    )
}

fn verifier_set_with_root(
    root: &Path,
    fixture: &TlsVerifierFixture,
    tls_root: Vec<u8>,
    secrets: Arc<SecretFacts>,
    seed: u64,
) -> EnterpriseIdentityProductionVerifiers {
    let metadata_path = root.join(format!("metadata-{seed}"));
    let mut metadata = SqliteStorage::open(&metadata_path).expect("open Credential metadata");
    let command = create_command(seed);
    CredentialReferenceService::new(&mut metadata)
        .create(&command, 1_700_000_000_000)
        .expect("create authority Credential reference");
    drop(metadata);
    EnterpriseIdentityProductionVerifiers::try_new(
        EnterpriseIdentityVerifierConfig::try_new(
            fixture.endpoint.clone(),
            EnterpriseIdentityVerifierTimeouts {
                connect: Duration::from_secs(2),
                response: Duration::from_secs(2),
                total: Duration::from_secs(5),
            },
            64 * 1024,
        )
        .expect("verifier config")
        .with_specific_tls_roots(vec![tls_root])
        .expect("verifier TLS root"),
        Box::new(SqliteStorage::open(metadata_path).expect("reopen Credential metadata")),
        secrets,
        command.scope,
        command.payload.credential_reference_id,
    )
    .expect("production verifier set")
}

#[test]
fn production_verifiers_use_tls_and_current_secret_boundary_without_leaking_protocol_material() {
    let root = TestDirectory::new("verified");
    let fixture = TlsVerifierFixture::start(ReplyBehavior::Verified, 3);
    let secrets = Arc::new(SecretFacts::default());
    let verifiers = verifier_set(&root.0, &fixture, Arc::clone(&secrets), 1);
    let public_debug = format!("{verifiers:?}");
    assert!(!public_debug.contains(&fixture.endpoint));
    assert!(
        !public_debug.contains(std::str::from_utf8(AUTHORITY_CREDENTIAL).expect("fixture UTF-8"))
    );
    let (mut oidc, mut saml, mut scim) = verifiers.into_verifiers();

    let oidc_claims = oidc
        .verify(std::str::from_utf8(OIDC_TOKEN).expect("OIDC fixture UTF-8"))
        .expect("verify OIDC token");
    assert_eq!(oidc_claims.subject, "subject-1");
    assert_eq!(oidc_claims.token_id, "oidc-token-1");
    let saml_claims = saml.verify(SAML_RESPONSE).expect("verify SAML response");
    assert_eq!(saml_claims.assertion_id, "saml-assertion-1");
    let scim_claims = scim
        .verify(std::str::from_utf8(SCIM_BEARER).expect("SCIM fixture UTF-8"))
        .expect("verify SCIM bearer");
    assert_eq!(scim_claims.client_id, "scim-client-1");
    assert_eq!(secrets.resolutions.load(Ordering::SeqCst), 3);

    let observations = fixture.finish();
    assert_eq!(
        observations,
        vec![
            RequestObservation {
                path: "/v1/identity/verify/oidc".to_owned(),
                operation: "verify_oidc".to_owned(),
                credential_matches: true,
                authority_matches: true,
                url_contains_credential: false,
            },
            RequestObservation {
                path: "/v1/identity/verify/saml".to_owned(),
                operation: "verify_saml".to_owned(),
                credential_matches: true,
                authority_matches: true,
                url_contains_credential: false,
            },
            RequestObservation {
                path: "/v1/identity/verify/scim".to_owned(),
                operation: "verify_scim".to_owned(),
                credential_matches: true,
                authority_matches: true,
                url_contains_credential: false,
            },
        ]
    );
    assert_files_omit(
        &root.0,
        &[AUTHORITY_CREDENTIAL, OIDC_TOKEN, SAML_RESPONSE, SCIM_BEARER],
    );
}

#[test]
fn remote_signature_rejection_and_noncanonical_claims_use_secret_safe_errors() {
    let root = TestDirectory::new("errors");
    let rejected_fixture = TlsVerifierFixture::start(ReplyBehavior::SignatureRejected, 1);
    let rejected = verifier_set(
        &root.0,
        &rejected_fixture,
        Arc::new(SecretFacts::default()),
        2,
    );
    let (mut oidc, _, _) = rejected.into_verifiers();
    let error = oidc
        .verify(std::str::from_utf8(OIDC_TOKEN).expect("OIDC fixture UTF-8"))
        .expect_err("signature rejection");
    assert_eq!(
        error.kind(),
        ProtocolVerificationErrorKind::SignatureRejected
    );
    let public = format!("{error:?} {error}");
    for secret in [AUTHORITY_CREDENTIAL, OIDC_TOKEN, SAML_RESPONSE, SCIM_BEARER] {
        assert!(
            !public
                .as_bytes()
                .windows(secret.len())
                .any(|value| value == secret)
        );
    }
    rejected_fixture.finish();

    let malformed_fixture = TlsVerifierFixture::start(ReplyBehavior::NonCanonical, 1);
    let malformed = verifier_set(
        &root.0,
        &malformed_fixture,
        Arc::new(SecretFacts::default()),
        3,
    );
    let (_, mut saml, _) = malformed.into_verifiers();
    assert_eq!(
        saml.verify(SAML_RESPONSE)
            .expect_err("noncanonical response")
            .kind(),
        ProtocolVerificationErrorKind::InvalidMessage
    );
    malformed_fixture.finish();
}

#[test]
fn untrusted_tls_authority_fails_closed_before_any_protocol_response_is_accepted() {
    let root = TestDirectory::new("untrusted-tls");
    let authority = TlsVerifierFixture::start(ReplyBehavior::Verified, 1);
    let foreign_authority = TlsVerifierFixture::start(ReplyBehavior::Verified, 0);
    let verifiers = verifier_set_with_root(
        &root.0,
        &authority,
        foreign_authority.certificate_der.clone(),
        Arc::new(SecretFacts::default()),
        30,
    );
    let (mut oidc, _, _) = verifiers.into_verifiers();
    assert_eq!(
        oidc.verify(std::str::from_utf8(OIDC_TOKEN).expect("OIDC fixture UTF-8"))
            .expect_err("untrusted TLS authority")
            .kind(),
        ProtocolVerificationErrorKind::KeyUnavailable
    );
    assert!(authority.finish().is_empty());
    assert!(foreign_authority.finish().is_empty());
}

#[test]
fn revoked_authority_reference_fails_before_secret_or_network_access() {
    let root = TestDirectory::new("revoked");
    let fixture = TlsVerifierFixture::start(ReplyBehavior::Verified, 0);
    let seed = 4;
    let metadata_path = root.0.join("metadata-revoked");
    let mut metadata = SqliteStorage::open(&metadata_path).expect("open Credential metadata");
    let create = create_command(seed);
    CredentialReferenceService::new(&mut metadata)
        .create(&create, 1_700_000_000_000)
        .expect("create authority Credential reference");
    CredentialReferenceService::new(&mut metadata)
        .revoke(
            &CredentialReferenceRevokeCommand {
                actor: create.actor.clone(),
                command: CredentialReferenceRevokeCommandCommand::CredentialReferenceRevoke,
                expected_revision: Revision(1),
                payload: CredentialReferenceRevokePayload {
                    credential_reference_id: create.payload.credential_reference_id.clone(),
                },
                request_id: RequestId(id("req", 40)),
                schema_version: SchemaVersion::WinwincodeV1,
                scope: create.scope.clone(),
            },
            1_700_000_001_000,
        )
        .expect("revoke authority Credential reference");
    drop(metadata);
    let secrets = Arc::new(SecretFacts::default());
    let verifiers = EnterpriseIdentityProductionVerifiers::try_new(
        EnterpriseIdentityVerifierConfig::try_new(
            fixture.endpoint.clone(),
            EnterpriseIdentityVerifierTimeouts {
                connect: Duration::from_secs(1),
                response: Duration::from_secs(1),
                total: Duration::from_secs(2),
            },
            64 * 1024,
        )
        .expect("verifier config")
        .with_specific_tls_roots(vec![fixture.certificate_der.clone()])
        .expect("verifier TLS root"),
        Box::new(SqliteStorage::open(metadata_path).expect("reopen Credential metadata")),
        secrets.clone(),
        create.scope,
        create.payload.credential_reference_id,
    )
    .expect("production verifier set");
    let (_, _, mut scim) = verifiers.into_verifiers();
    assert_eq!(
        scim.verify(std::str::from_utf8(SCIM_BEARER).expect("SCIM fixture UTF-8"))
            .expect_err("revoked reference")
            .kind(),
        ProtocolVerificationErrorKind::KeyUnavailable
    );
    assert_eq!(secrets.resolutions.load(Ordering::SeqCst), 0);
    fixture.finish();
}

#[test]
#[ignore = "requires a real IdP artifact set, verifier authority, Credential reference, SecretStore, and TLS root"]
fn live_real_oidc_saml_scim_verifier_gate_requires_explicit_file_backed_credentials() {
    let endpoint = required_environment("WINWINCODE_LIVE_IDENTITY_VERIFIER_ENDPOINT");
    let metadata_directory = PathBuf::from(required_environment(
        "WINWINCODE_LIVE_IDENTITY_METADATA_DIRECTORY",
    ));
    let secret_directory = PathBuf::from(required_environment(
        "WINWINCODE_LIVE_IDENTITY_SECRET_DIRECTORY",
    ));
    let tls_root_file = PathBuf::from(required_environment(
        "WINWINCODE_LIVE_IDENTITY_TLS_ROOT_DER_FILE",
    ));
    let oidc_file = PathBuf::from(required_environment("WINWINCODE_LIVE_OIDC_ID_TOKEN_FILE"));
    let saml_file = PathBuf::from(required_environment("WINWINCODE_LIVE_SAML_RESPONSE_FILE"));
    let scim_file = PathBuf::from(required_environment("WINWINCODE_LIVE_SCIM_BEARER_FILE"));
    let evidence_file = PathBuf::from(required_environment(
        "WINWINCODE_LIVE_IDENTITY_EVIDENCE_FILE",
    ));
    let organization_id = OrganizationId(required_environment(
        "WINWINCODE_LIVE_IDENTITY_ORGANIZATION_ID",
    ));
    let reference_id = CredentialReferenceId(required_environment(
        "WINWINCODE_LIVE_IDENTITY_VERIFIER_CREDENTIAL_REFERENCE_ID",
    ));
    let tls_root = fs::read(tls_root_file).expect("read live verifier TLS root file");
    let metadata = SqliteStorage::open(&metadata_directory).expect("open live Credential metadata");
    let secrets: Arc<dyn SecretStorePort> =
        Arc::new(LocalSecretStoreAdapter::open(secret_directory).expect("open live SecretStore"));
    let verifiers = EnterpriseIdentityProductionVerifiers::try_new(
        EnterpriseIdentityVerifierConfig::try_new(
            endpoint,
            EnterpriseIdentityVerifierTimeouts {
                connect: Duration::from_secs(5),
                response: Duration::from_secs(10),
                total: Duration::from_secs(30),
            },
            64 * 1024,
        )
        .expect("live verifier config")
        .with_specific_tls_roots(vec![tls_root])
        .expect("live verifier TLS root"),
        Box::new(metadata),
        secrets,
        Scope::OrganizationScope(OrganizationScope {
            kind: OrganizationScopeKind::Organization,
            organization_id,
        }),
        reference_id,
    )
    .expect("live production verifier set");
    let (mut oidc, mut saml, mut scim) = verifiers.into_verifiers();
    let mut oidc_bytes = fs::read(oidc_file).expect("read live OIDC ID token file");
    let oidc_text = std::str::from_utf8(&oidc_bytes).expect("OIDC ID token must be UTF-8");
    let oidc_claims = oidc
        .verify(oidc_text)
        .expect("live OIDC ID token verification");
    assert!(!oidc_claims.issuer.is_empty());
    let mut saml_bytes = fs::read(saml_file).expect("read live SAML response file");
    let saml_claims = saml
        .verify(&saml_bytes)
        .expect("live signed SAML response verification");
    assert!(!saml_claims.issuer.is_empty());
    let mut scim_bytes = fs::read(scim_file).expect("read live SCIM bearer file");
    let scim_text = std::str::from_utf8(&scim_bytes).expect("SCIM bearer must be UTF-8");
    let scim_claims = scim
        .verify(scim_text)
        .expect("live SCIM bearer verification");
    assert!(!scim_claims.client_id.is_empty());
    let evidence = live_evidence(&oidc_claims, &saml_claims, &scim_claims);
    write_live_evidence(
        &evidence_file,
        &evidence,
        &[&oidc_bytes, &saml_bytes, &scim_bytes],
    );
    assert_files_omit(
        &metadata_directory,
        &[&oidc_bytes, &saml_bytes, &scim_bytes],
    );
    oidc_bytes.fill(0);
    saml_bytes.fill(0);
    scim_bytes.fill(0);
}

fn required_environment(name: &str) -> String {
    std::env::var(name)
        .ok()
        .filter(|value| !value.is_empty())
        .unwrap_or_else(|| panic!("{name} is required"))
}

fn live_evidence(
    oidc: &winwincode_control_plane::VerifiedOidcClaims,
    saml: &winwincode_control_plane::VerifiedSamlClaims,
    scim: &winwincode_control_plane::VerifiedScimClient,
) -> Vec<u8> {
    serde_json::to_vec(&json!({
        "schema": "winwincode.enterprise-identity-live-verifier-evidence.v1",
        "oidc": {
            "verified": true,
            "issuerSha256": digest_hex(oidc.issuer.as_bytes()),
            "subjectSha256": digest_hex(oidc.subject.as_bytes()),
            "tokenIdSha256": digest_hex(oidc.token_id.as_bytes())
        },
        "saml": {
            "verified": true,
            "issuerSha256": digest_hex(saml.issuer.as_bytes()),
            "subjectSha256": digest_hex(saml.subject.as_bytes()),
            "assertionIdSha256": digest_hex(saml.assertion_id.as_bytes())
        },
        "scim": {
            "verified": true,
            "issuerSha256": digest_hex(scim.issuer.as_bytes()),
            "clientIdSha256": digest_hex(scim.client_id.as_bytes())
        }
    }))
    .expect("serialize secret-free live evidence")
}

fn write_live_evidence(path: &Path, evidence: &[u8], restricted: &[&[u8]]) {
    for value in restricted {
        assert!(
            !evidence.windows(value.len()).any(|window| window == *value),
            "live protocol material reached verifier evidence"
        );
    }
    let mut output = OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .mode(0o600)
        .open(path)
        .expect("open live evidence file");
    output
        .write_all(evidence)
        .expect("write live evidence file");
    output.sync_all().expect("sync live evidence file");
}

fn assert_files_omit(root: &Path, restricted: &[&[u8]]) {
    let mut pending = vec![root.to_path_buf()];
    while let Some(path) = pending.pop() {
        for entry in fs::read_dir(path).expect("read verifier fixture directory") {
            let entry = entry.expect("verifier fixture entry");
            let kind = entry.file_type().expect("verifier fixture type");
            if kind.is_dir() {
                pending.push(entry.path());
                continue;
            }
            if !kind.is_file() {
                continue;
            }
            let bytes = fs::read(entry.path()).expect("read verifier fixture file");
            for value in restricted {
                assert!(
                    !bytes.windows(value.len()).any(|window| window == *value),
                    "protocol or authority credential reached a durable file"
                );
            }
        }
    }
}

fn digest_hex(bytes: &[u8]) -> String {
    use sha2::{Digest as _, Sha256};
    format!("sha256:{:x}", Sha256::digest(bytes))
}
