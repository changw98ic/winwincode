// SPDX-License-Identifier: Apache-2.0

use std::collections::BTreeSet;
use std::fs;
use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use base64::{Engine as _, engine::general_purpose::STANDARD};
use futures::{SinkExt, StreamExt};
use serde_json::{Value, json};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;
use tokio::sync::mpsc;
use tokio_tungstenite::client_async;
use tokio_tungstenite::tungstenite::client::IntoClientRequest;
use winwincode_api::generated::{
    Actor, EnterpriseOrganizationUpdateCommand, EnterpriseOrganizationUpdateCommandCommand,
    EnterpriseOrganizationUpdatePayload, OrganizationScope, OrganizationScopeKind, Scope,
    ServiceAccountActor, ServiceAccountActorKind,
};
use winwincode_control_plane::{
    CanonicalEnterpriseIdentityLifecycle, EnterpriseIdentityClock, EnterpriseIdentityClockError,
    EnterpriseIdentityLifecyclePort, EnterpriseIdentityProtocolAdapter,
    EnterpriseIdentityProtocolConfig, EnterpriseIdentityService, EnterpriseProtocolClock,
    EnterpriseProtocolClockError, EnterpriseRbacClock, EnterpriseRbacClockError,
    EnterpriseRbacService, ExternalIdentityProvider, ExternalIdentityReference, OidcTokenVerifier,
    ProtocolVerificationError, ProvisionExternalUser, SamlResponseVerifier, ScimBearerVerifier,
    ScimLifecycleEvent, ScimOperation, ScimUserDeprovision, ScimUserProvision,
    TrustedProtocolParty, VerifiedOidcClaims, VerifiedSamlClaims, VerifiedScimClient,
};
use winwincode_domain::{
    OrganizationId, RequestId, Revision, SchemaVersion, ServiceAccountId, Sha256Digest, UserId,
};
use winwincode_server::{
    ApiError, AuthSessionBootstrap, AuthSessionConfig, AuthenticatedPrincipal, ControlPlaneApiPort,
    EnterpriseIdentityProtocolApplication, EventSubscription, RequestAuthenticator, ServerConfig,
    ServerTls, SqliteAuthSessionManager, start_server,
};
use winwincode_storage::SqliteStorage;

const NOW: u64 = 1_700_000_000_000;
const OIDC_TOKEN: &str = "oidc-live-fixture-token";
const SAML_RESPONSE: &[u8] = b"saml-live-fixture-response";
const SCIM_BEARER: &str = "scim-live-fixture-bearer";
static NEXT_FIXTURE: AtomicU64 = AtomicU64::new(1);

#[derive(Clone, Copy)]
struct FixedClock;

impl EnterpriseIdentityClock for FixedClock {
    fn now_millis(&mut self) -> Result<u64, EnterpriseIdentityClockError> {
        Ok(NOW)
    }
}

impl EnterpriseRbacClock for FixedClock {
    fn now_millis(&mut self) -> Result<u64, EnterpriseRbacClockError> {
        Ok(NOW)
    }
}

impl EnterpriseProtocolClock for FixedClock {
    fn now_millis(&mut self) -> Result<u64, EnterpriseProtocolClockError> {
        Ok(NOW)
    }
}

struct FakeOidcVerifier;

impl OidcTokenVerifier for FakeOidcVerifier {
    fn verify(&mut self, token: &str) -> Result<VerifiedOidcClaims, ProtocolVerificationError> {
        if token == "oidc-bad-signature" {
            return Err(ProtocolVerificationError::signature_rejected());
        }
        Ok(VerifiedOidcClaims {
            issuer: if token == "oidc-wrong-issuer" {
                "https://foreign.example".to_owned()
            } else {
                "https://idp.example/oidc".to_owned()
            },
            audiences: if token == "oidc-wrong-audience" {
                vec!["foreign".to_owned()]
            } else {
                vec!["winwincode".to_owned()]
            },
            subject: "subject-1".to_owned(),
            token_id: "oidc-token-1".to_owned(),
            issued_at_millis: NOW - 1_000,
            not_before_millis: NOW - 1_000,
            expires_at_millis: NOW + 60_000,
        })
    }
}

struct FakeSamlVerifier;

impl SamlResponseVerifier for FakeSamlVerifier {
    fn verify(&mut self, response: &[u8]) -> Result<VerifiedSamlClaims, ProtocolVerificationError> {
        if response == b"saml-bad-signature" {
            return Err(ProtocolVerificationError::signature_rejected());
        }
        Ok(VerifiedSamlClaims {
            issuer: "https://idp.example/saml".to_owned(),
            audiences: vec!["winwincode".to_owned()],
            subject: "subject-1".to_owned(),
            assertion_id: "saml-assertion-1".to_owned(),
            issued_at_millis: NOW - 1_000,
            not_before_millis: NOW - 1_000,
            expires_at_millis: NOW + 60_000,
        })
    }
}

struct FakeScimVerifier;

impl ScimBearerVerifier for FakeScimVerifier {
    fn verify(&mut self, bearer: &str) -> Result<VerifiedScimClient, ProtocolVerificationError> {
        if bearer != SCIM_BEARER {
            return Err(ProtocolVerificationError::signature_rejected());
        }
        Ok(VerifiedScimClient {
            issuer: "https://idp.example/scim".to_owned(),
            audiences: vec!["winwincode-scim".to_owned()],
            client_id: "scim-client-1".to_owned(),
            expires_at_millis: NOW + 60_000,
        })
    }
}

#[derive(Default)]
struct FakeApi {
    event_senders: Mutex<Vec<mpsc::Sender<Value>>>,
}

impl ControlPlaneApiPort for FakeApi {
    fn command(
        &self,
        _principal: &AuthenticatedPrincipal,
        request: Value,
    ) -> Result<Value, ApiError> {
        Ok(json!({ "kind": "command_result", "request": request }))
    }

    fn query(
        &self,
        _principal: &AuthenticatedPrincipal,
        request: Value,
    ) -> Result<Value, ApiError> {
        Ok(json!({ "kind": "query_result", "request": request }))
    }

    fn subscribe(
        &self,
        _principal: &AuthenticatedPrincipal,
        _first_frame: Value,
    ) -> Result<EventSubscription, ApiError> {
        let (sender, receiver) = mpsc::channel(1);
        sender
            .try_send(json!({ "type": "event.v1", "sequence": 1 }))
            .expect("seed event");
        self.event_senders
            .lock()
            .expect("event senders")
            .push(sender);
        Ok(EventSubscription {
            initial_frames: vec![json!({
                "type": "transport.subscription-accepted.v1"
            })],
            events: receiver,
        })
    }

    fn event_control(
        &self,
        _principal: &AuthenticatedPrincipal,
        _frame: Value,
    ) -> Result<Vec<Value>, ApiError> {
        Ok(Vec::new())
    }

    fn shutdown(&self) -> Result<(), ApiError> {
        Ok(())
    }
}

struct Fixture {
    root: PathBuf,
    organization_id: OrganizationId,
    user_id: UserId,
    identity: Arc<EnterpriseIdentityService>,
    rbac: Arc<EnterpriseRbacService>,
    sessions: Arc<SqliteAuthSessionManager>,
    api: Arc<FakeApi>,
}

impl Fixture {
    fn new() -> Self {
        let serial = NEXT_FIXTURE.fetch_add(1, Ordering::SeqCst);
        let root = std::env::temp_dir().join(format!(
            "winwincode-enterprise-identity-http-{}-{serial}",
            std::process::id()
        ));
        let _ = fs::remove_dir_all(&root);
        fs::create_dir_all(&root).expect("create identity HTTP fixture");
        let organization_id = OrganizationId("org_00000000000000000000000001".to_owned());
        let user_id = UserId("usr_00000000000000000000000001".to_owned());
        let identity = Arc::new(EnterpriseIdentityService::with_clock(
            Box::new(SqliteStorage::open(&root).expect("identity storage")),
            Box::new(FixedClock),
        ));
        let rbac = Arc::new(EnterpriseRbacService::with_clock(
            Box::new(SqliteStorage::open(&root).expect("RBAC storage")),
            Box::new(FixedClock),
        ));
        let sessions = Arc::new(
            SqliteAuthSessionManager::open(
                root.join("auth-sessions"),
                vec![
                    AuthSessionBootstrap::new(
                        "fixture-bootstrap-proof",
                        user_actor(&user_id),
                        vec![organization_scope(&organization_id)],
                    )
                    .expect("bootstrap"),
                ],
                AuthSessionConfig::default(),
            )
            .expect("session manager"),
        );
        let fixture = Self {
            root,
            organization_id,
            user_id,
            identity,
            rbac,
            sessions,
            api: Arc::new(FakeApi::default()),
        };
        fixture.seed_organization();
        fixture.provision_login_mappings();
        fixture
    }

    fn seed_organization(&self) {
        self.rbac
            .update_organization(&EnterpriseOrganizationUpdateCommand {
                actor: management_actor(),
                command: EnterpriseOrganizationUpdateCommandCommand::EnterpriseOrganizationUpdate,
                expected_revision: Revision(0),
                payload: EnterpriseOrganizationUpdatePayload {
                    display_name: "Example Organization".to_owned(),
                    organization_id: self.organization_id.clone(),
                    slug: "example".to_owned(),
                    state: "active".to_owned(),
                },
                request_id: request_id(1),
                schema_version: SchemaVersion::WinwincodeV1,
                scope: organization_scope(&self.organization_id),
            })
            .expect("seed Organization");
    }

    fn lifecycle(&self) -> CanonicalEnterpriseIdentityLifecycle {
        CanonicalEnterpriseIdentityLifecycle::new(
            Arc::clone(&self.identity),
            Arc::clone(&self.rbac),
            self.sessions.clone(),
            management_actor(),
        )
    }

    fn provision_login_mappings(&self) {
        for (provider, issuer, operation) in [
            (
                ExternalIdentityProvider::Oidc,
                "https://idp.example/oidc",
                "provision-oidc",
            ),
            (
                ExternalIdentityProvider::Saml,
                "https://idp.example/saml",
                "provision-saml",
            ),
        ] {
            self.lifecycle()
                .provision_user(&ProvisionExternalUser {
                    operation_id: operation.to_owned(),
                    identity: ExternalIdentityReference {
                        organization_id: self.organization_id.clone(),
                        provider,
                        issuer_sha256: sha256(issuer.as_bytes()),
                        subject_sha256: sha256(b"subject-1"),
                    },
                    user_id: self.user_id.clone(),
                    display_name: "Ada Example".to_owned(),
                    authorized_scopes: vec![organization_scope(&self.organization_id)],
                    team_ids: Vec::new(),
                    role_assignments: Vec::new(),
                })
                .expect("provision login mapping");
        }
    }

    fn protocol_application(&self) -> Arc<EnterpriseIdentityProtocolApplication> {
        let adapter = EnterpriseIdentityProtocolAdapter::with_clock(
            Box::new(SqliteStorage::open(&self.root).expect("protocol storage")),
            Box::new(self.lifecycle()),
            Box::new(FakeOidcVerifier),
            Box::new(FakeSamlVerifier),
            Box::new(FakeScimVerifier),
            Box::new(FixedClock),
            EnterpriseIdentityProtocolConfig {
                organization_id: self.organization_id.clone(),
                management_actor: management_actor(),
                oidc: TrustedProtocolParty {
                    issuer: "https://idp.example/oidc".to_owned(),
                    audience: "winwincode".to_owned(),
                },
                saml: TrustedProtocolParty {
                    issuer: "https://idp.example/saml".to_owned(),
                    audience: "winwincode".to_owned(),
                },
                scim: TrustedProtocolParty {
                    issuer: "https://idp.example/scim".to_owned(),
                    audience: "winwincode-scim".to_owned(),
                },
                max_clock_skew_millis: 5_000,
                max_assertion_age_millis: 300_000,
            },
        )
        .expect("protocol adapter");
        Arc::new(EnterpriseIdentityProtocolApplication::new(
            Arc::new(adapter),
            Arc::clone(&self.sessions),
        ))
    }

    async fn start(&self) -> winwincode_server::RunningServer {
        let config = ServerConfig::new(
            "127.0.0.1:0".parse().expect("address"),
            "http://control.example",
            ServerTls::Disabled,
            BTreeSet::from(["https://client.example".to_owned()]),
            self.root.join("server"),
            Duration::from_secs(2),
        )
        .expect("server config");
        let authenticator: Arc<dyn RequestAuthenticator> = self.sessions.clone();
        let api: Arc<dyn ControlPlaneApiPort> = self.api.clone();
        start_server(
            config,
            Arc::clone(&self.sessions),
            authenticator,
            api,
            Some(self.protocol_application()),
        )
        .await
        .expect("start identity server")
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.root);
    }
}

#[tokio::test]
async fn oidc_and_saml_callbacks_issue_one_session_and_reject_changed_trust_without_echoing_input()
{
    let fixture = Fixture::new();
    let first = fixture.start().await;
    let address = first.local_address();
    let oidc = oidc_callback(address, OIDC_TOKEN).await;
    assert!(oidc.starts_with("HTTP/1.1 201 Created"), "{oidc}");
    assert!(oidc.contains("cache-control: no-store"), "{oidc}");
    assert!(!oidc.contains(OIDC_TOKEN));
    let oidc_cookie = session_cookie(&oidc);
    let current = current_session(address, &oidc_cookie).await;
    assert!(current.starts_with("HTTP/1.1 200 OK"), "{current}");

    first.shutdown().await.expect("first shutdown");
    let restarted = fixture.start().await;
    let replay = oidc_callback(restarted.local_address(), OIDC_TOKEN).await;
    assert!(replay.starts_with("HTTP/1.1 200 OK"), "{replay}");
    assert!(!replay.contains("set-cookie:"));
    assert!(replay.contains("external_identity_callback_replay"));
    assert!(!replay.contains(OIDC_TOKEN));

    assert_rejected_oidc_callbacks(restarted.local_address()).await;
    assert_saml_callbacks(restarted.local_address()).await;
    restarted.shutdown().await.expect("restarted shutdown");
}

#[tokio::test]
async fn scim_deprovision_revokes_existing_http_and_websocket_session() {
    let fixture = Fixture::new();
    let running = fixture.start().await;
    let address = running.local_address();
    let oidc = oidc_callback(address, OIDC_TOKEN).await;
    let cookie = session_cookie(&oidc);

    let provisioned = scim_callback(address, &provision_event(&fixture)).await;
    assert!(
        provisioned.starts_with("HTTP/1.1 204 No Content"),
        "{provisioned}"
    );
    assert!(!provisioned.contains(SCIM_BEARER));

    let mut socket = connect_websocket(address, &cookie).await;
    socket
        .send(tokio_tungstenite::tungstenite::Message::Text(
            r#"{"type":"transport.subscribe.v1"}"#.into(),
        ))
        .await
        .expect("subscribe");
    socket
        .next()
        .await
        .expect("accepted")
        .expect("accepted frame");
    socket.next().await.expect("event").expect("event frame");

    let deprovision = deprovision_event(&fixture, "scim-user-deprovision-2", 2);
    let deprovisioned = scim_callback(address, &deprovision).await;
    assert!(
        deprovisioned.starts_with("HTTP/1.1 204 No Content"),
        "{deprovisioned}"
    );
    assert!(!deprovisioned.contains(SCIM_BEARER));

    let current = current_session(address, &cookie).await;
    assert!(
        current.starts_with("HTTP/1.1 401 Unauthorized"),
        "{current}"
    );
    assert!(!current.contains(&cookie));
    let closed = tokio::time::timeout(Duration::from_secs(2), socket.next())
        .await
        .expect("session revocation close deadline")
        .expect("session revocation close frame")
        .expect("valid session revocation close frame");
    let tokio_tungstenite::tungstenite::Message::Close(Some(frame)) = closed else {
        panic!("SCIM deprovision must close the WebSocket session");
    };
    assert_eq!(u16::from(frame.code), 4403);

    let repeated = scim_callback(address, &deprovision).await;
    assert!(
        repeated.starts_with("HTTP/1.1 204 No Content"),
        "{repeated}"
    );
    let out_of_order = deprovision_event(&fixture, "scim-user-out-of-order", 1);
    let rejected = scim_callback(address, &out_of_order).await;
    assert!(rejected.starts_with("HTTP/1.1 409 Conflict"), "{rejected}");
    assert!(!rejected.contains(SCIM_BEARER));
    running.shutdown().await.expect("shutdown");
}

async fn oidc_callback(address: SocketAddr, token: &str) -> String {
    let body = serde_json::to_string(&json!({
        "schemaVersion": "winwincode/v1",
        "idToken": token
    }))
    .expect("OIDC callback JSON");
    http_request(
        address,
        &request(
            "POST",
            "/api/v1/auth/oidc/callback",
            "application/json",
            None,
            &body,
        ),
    )
    .await
}

async fn assert_rejected_oidc_callbacks(address: SocketAddr) {
    for token in [
        "oidc-bad-signature",
        "oidc-wrong-issuer",
        "oidc-wrong-audience",
    ] {
        let response = oidc_callback(address, token).await;
        assert!(
            response.starts_with("HTTP/1.1 401 Unauthorized"),
            "{response}"
        );
        assert!(!response.contains(token));
    }
}

async fn assert_saml_callbacks(address: SocketAddr) {
    let encoded = STANDARD.encode(SAML_RESPONSE);
    let saml = saml_callback(address, &format!("{encoded}&RelayState=fixture-state")).await;
    assert!(saml.starts_with("HTTP/1.1 201 Created"), "{saml}");
    assert!(!saml.contains(&encoded));
    assert!(!saml.contains("fixture-state"));

    let rejected_encoded = STANDARD.encode(b"saml-bad-signature");
    let rejected = saml_callback(address, &rejected_encoded).await;
    assert!(
        rejected.starts_with("HTTP/1.1 401 Unauthorized"),
        "{rejected}"
    );
    assert!(!rejected.contains(&rejected_encoded));
}

async fn saml_callback(address: SocketAddr, fields: &str) -> String {
    http_request(
        address,
        &request(
            "POST",
            "/api/v1/auth/saml/acs",
            "application/x-www-form-urlencoded",
            None,
            &format!("SAMLResponse={fields}"),
        ),
    )
    .await
}

async fn scim_callback(address: SocketAddr, event: &ScimLifecycleEvent) -> String {
    let body = serde_json::to_string(event).expect("SCIM event JSON");
    http_request(
        address,
        &request(
            "POST",
            "/api/v1/scim/events",
            "application/json",
            Some(SCIM_BEARER),
            &body,
        ),
    )
    .await
}

fn provision_event(fixture: &Fixture) -> ScimLifecycleEvent {
    ScimLifecycleEvent {
        event_id: "scim-user-provision-1".to_owned(),
        sequence: 1,
        operation: ScimOperation::ProvisionUser(ScimUserProvision {
            external_subject: "subject-1".to_owned(),
            user_id: fixture.user_id.clone(),
            display_name: "Ada Example".to_owned(),
            authorized_scopes: vec![organization_scope(&fixture.organization_id)],
            team_ids: Vec::new(),
            role_assignments: Vec::new(),
        }),
    }
}

fn deprovision_event(fixture: &Fixture, event_id: &str, sequence: u64) -> ScimLifecycleEvent {
    ScimLifecycleEvent {
        event_id: event_id.to_owned(),
        sequence,
        operation: ScimOperation::DeprovisionUser(ScimUserDeprovision {
            external_subject: "subject-1".to_owned(),
            user_id: fixture.user_id.clone(),
        }),
    }
}

async fn http_request(address: SocketAddr, request: &str) -> String {
    let mut stream = TcpStream::connect(address).await.expect("connect server");
    stream
        .write_all(request.as_bytes())
        .await
        .expect("write request");
    let mut response = Vec::new();
    stream
        .read_to_end(&mut response)
        .await
        .expect("read response");
    String::from_utf8(response).expect("HTTP response")
}

fn request(
    method: &str,
    path: &str,
    content_type: &str,
    bearer: Option<&str>,
    body: &str,
) -> String {
    let authorization = bearer.map_or_else(String::new, |bearer| {
        format!("Authorization: Bearer {bearer}\r\n")
    });
    format!(
        "{method} {path} HTTP/1.1\r\nHost: control.example\r\n{authorization}Content-Type: {content_type}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    )
}

async fn current_session(address: SocketAddr, cookie: &str) -> String {
    http_request(
        address,
        &format!(
            "GET /api/v1/auth/session HTTP/1.1\r\nHost: control.example\r\nOrigin: https://client.example\r\nCookie: wwc_session={cookie}\r\nConnection: close\r\n\r\n"
        ),
    )
    .await
}

fn session_cookie(response: &str) -> String {
    assert!(response.starts_with("HTTP/1.1 201 Created"), "{response}");
    response
        .lines()
        .find_map(|line| line.strip_prefix("set-cookie: "))
        .and_then(|value| value.split(';').next())
        .and_then(|pair| pair.strip_prefix("wwc_session="))
        .expect("session cookie")
        .to_owned()
}

async fn connect_websocket(
    address: SocketAddr,
    session_cookie: &str,
) -> tokio_tungstenite::WebSocketStream<TcpStream> {
    let mut request = format!("ws://{address}/api/v1/events")
        .into_client_request()
        .expect("WebSocket request");
    request.headers_mut().insert(
        "Cookie",
        format!("wwc_session={session_cookie}")
            .parse()
            .expect("cookie header"),
    );
    request.headers_mut().insert(
        "Origin",
        "https://client.example".parse().expect("origin header"),
    );
    let stream = TcpStream::connect(address)
        .await
        .expect("connect WebSocket");
    let (socket, response) = client_async(request, stream)
        .await
        .expect("upgrade WebSocket");
    assert_eq!(response.status(), 101);
    socket
}

fn management_actor() -> Actor {
    Actor::ServiceAccountActor(ServiceAccountActor {
        kind: ServiceAccountActorKind::ServiceAccount,
        id: ServiceAccountId("svc_00000000000000000000000001".to_owned()),
    })
}

fn user_actor(user_id: &UserId) -> Actor {
    Actor::UserActor(winwincode_api::generated::UserActor {
        kind: winwincode_api::generated::UserActorKind::User,
        id: user_id.clone(),
    })
}

fn organization_scope(organization_id: &OrganizationId) -> Scope {
    Scope::OrganizationScope(OrganizationScope {
        kind: OrganizationScopeKind::Organization,
        organization_id: organization_id.clone(),
    })
}

fn request_id(seed: u64) -> RequestId {
    RequestId(format!("req_{seed:026}"))
}

fn sha256(bytes: &[u8]) -> Sha256Digest {
    use sha2::{Digest as _, Sha256};
    Sha256Digest(format!("sha256:{:x}", Sha256::digest(bytes)))
}
