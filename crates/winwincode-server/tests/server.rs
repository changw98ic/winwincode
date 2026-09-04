// SPDX-License-Identifier: Apache-2.0

use std::collections::BTreeSet;
use std::fs;
use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::Duration;

use futures::{SinkExt, StreamExt};
use rcgen::CertifiedKey;
use rcgen::generate_simple_self_signed;
use rustls::ClientConfig;
use rustls::RootCertStore;
use rustls::pki_types::ServerName;
use serde_json::{Value, json};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;
use tokio::sync::mpsc;
use tokio_rustls::TlsConnector;
use tokio_tungstenite::client_async;
use tokio_tungstenite::tungstenite::client::IntoClientRequest;
use winwincode_api::generated::{
    Actor, ControlPlaneWebSocketProtocolErrorFrame, EnterpriseIdentityUpdateCommand, Error,
    ErrorDetailValue, ErrorEnvelope, OrganizationScope, OrganizationScopeKind, RetryableErrorCode,
    Scope, TerminalErrorCode,
};
use winwincode_control_plane::{EnterpriseIdentityService, generate_api_token};
use winwincode_domain::{ApiTokenId, OrganizationId, UserId};
use winwincode_domain::{UserActor, UserActorKind};
use winwincode_server::{
    ApiError, AuthSessionBootstrap, AuthSessionConfig, AuthenticatedPrincipal, ControlPlaneApiPort,
    EnterpriseRequestAuthenticator, EventSubscription, RequestAuthenticator, RunningServer,
    ServerConfig, ServerError, ServerTls, SqliteAuthSessionManager,
    start_server as start_server_with_authenticator,
};
use winwincode_storage::SqliteStorage;

const FIXTURE_SECRET: &str = "sk-fixturecredentialleakgate1234567890";
const BOOTSTRAP_PROOF: &str = "test-bootstrap-proof";

static NEXT_DIRECTORY: AtomicU64 = AtomicU64::new(1);
static TEST_RUN_NAMESPACE: OnceLock<String> = OnceLock::new();

fn test_directory(label: &str) -> PathBuf {
    let namespace = TEST_RUN_NAMESPACE.get_or_init(|| {
        const HEX: &[u8; 16] = b"0123456789abcdef";
        let mut nonce = [0_u8; 16];
        getrandom::fill(&mut nonce).expect("test run namespace entropy");
        let mut encoded = String::with_capacity(nonce.len() * 2);
        for byte in nonce {
            encoded.push(char::from(HEX[usize::from(byte >> 4)]));
            encoded.push(char::from(HEX[usize::from(byte & 0x0f)]));
        }
        format!("{}-{encoded}", std::process::id())
    });
    let id = NEXT_DIRECTORY.fetch_add(1, Ordering::Relaxed);
    std::env::temp_dir().join(format!("{label}-{namespace}-{id}"))
}

fn fixture_actor(seed: u8) -> Actor {
    Actor::UserActor(UserActor {
        kind: UserActorKind::User,
        id: UserId(format!("usr_{seed:026}")),
    })
}

fn fixture_scope(seed: u8) -> Scope {
    Scope::OrganizationScope(OrganizationScope {
        kind: OrganizationScopeKind::Organization,
        organization_id: OrganizationId(format!("org_{seed:026}")),
    })
}

fn fixture_bootstrap(proof: &str, seed: u8, scopes: Vec<Scope>) -> AuthSessionBootstrap {
    AuthSessionBootstrap::new(proof, fixture_actor(seed), scopes).expect("bootstrap context")
}

#[derive(Default)]
struct FakeApi {
    commands: Mutex<Vec<(String, Value)>>,
    queries: Mutex<Vec<(String, Value)>>,
    subscriptions: Mutex<Vec<Value>>,
    event_senders: Mutex<Vec<mpsc::Sender<Value>>>,
    shutdowns: AtomicU64,
}

impl ControlPlaneApiPort for FakeApi {
    fn command(
        &self,
        principal: &AuthenticatedPrincipal,
        request: Value,
    ) -> Result<Value, ApiError> {
        self.commands
            .lock()
            .expect("command lock")
            .push((principal.subject().to_owned(), request.clone()));
        if request.get("command").and_then(Value::as_str) == Some("fixture.leak") {
            return Ok(json!({
                "requestId": request.get("requestId"),
                "authorization": format!("Bearer {FIXTURE_SECRET}")
            }));
        }
        Ok(json!({ "kind": "command_result", "request": request }))
    }

    fn query(&self, principal: &AuthenticatedPrincipal, request: Value) -> Result<Value, ApiError> {
        self.queries
            .lock()
            .expect("query lock")
            .push((principal.subject().to_owned(), request.clone()));
        if request.get("query").and_then(Value::as_str) == Some("fixture.unavailable") {
            return Err(ApiError::new(
                503,
                "SERVICE_UNAVAILABLE",
                "service is temporarily unavailable",
            ));
        }
        if request.get("query").and_then(Value::as_str) == Some("fixture.invalid-status") {
            return Err(ApiError::new(
                200,
                "INTERNAL_ERROR",
                "application returned an invalid error status",
            ));
        }
        if request.get("query").and_then(Value::as_str) == Some("fixture.internal-message") {
            return Err(ApiError::new(
                500,
                "PROVIDER_ADAPTER_FAILED",
                "adapter failed while opening /private/var/winwincode/provider.sqlite3",
            ));
        }
        if request.get("query").and_then(Value::as_str) == Some("fixture.leak") {
            return Err(ApiError::new(
                502,
                "PROVIDER_FAILED",
                format!("provider returned Bearer {FIXTURE_SECRET}"),
            ));
        }
        Ok(json!({ "kind": "query_result", "request": request }))
    }

    fn subscribe(
        &self,
        principal: &AuthenticatedPrincipal,
        first_frame: Value,
    ) -> Result<EventSubscription, ApiError> {
        self.subscriptions
            .lock()
            .expect("subscription lock")
            .push(first_frame.clone());
        let (sender, receiver) = mpsc::channel(1);
        sender
            .try_send(json!({ "type": "event.v1", "sequence": 1 }))
            .expect("seed event");
        self.event_senders
            .lock()
            .expect("event senders")
            .push(sender);
        let initial_frames =
            if first_frame.get("type").and_then(Value::as_str) == Some("fixture.leak") {
                vec![json!({ "token": FIXTURE_SECRET })]
            } else if first_frame.get("type").and_then(Value::as_str)
                == Some("fixture.public-component")
            {
                vec![json!({
                    "type": "event.v1",
                    "source": {
                        "kind": "control-plane",
                        "component": "delivery-task-breakdown-transaction"
                    }
                })]
            } else {
                vec![json!({
                    "type": "transport.subscription-accepted.v1",
                    "subject": principal.subject(),
                    "request": first_frame
                })]
            };
        Ok(EventSubscription {
            initial_frames,
            events: receiver,
        })
    }

    fn event_control(
        &self,
        _principal: &AuthenticatedPrincipal,
        frame: Value,
    ) -> Result<Vec<Value>, ApiError> {
        Ok(vec![
            json!({ "type": "transport.control-accepted.v1", "request": frame }),
        ])
    }

    fn shutdown(&self) -> Result<(), ApiError> {
        self.shutdowns.fetch_add(1, Ordering::Relaxed);
        Ok(())
    }
}

fn config(bind_address: SocketAddr) -> ServerConfig {
    ServerConfig::new(
        bind_address,
        "http://control.example",
        ServerTls::Disabled,
        BTreeSet::from(["https://client.example".to_owned()]),
        test_directory("winwincode-server-test"),
        Duration::from_secs(2),
    )
    .expect("valid config")
}

fn auth() -> Arc<SqliteAuthSessionManager> {
    Arc::new(
        SqliteAuthSessionManager::open(
            test_directory("winwincode-auth-test"),
            vec![fixture_bootstrap(
                BOOTSTRAP_PROOF,
                1,
                vec![fixture_scope(1)],
            )],
            AuthSessionConfig::default(),
        )
        .expect("authenticator"),
    )
}

async fn start_server(
    config: ServerConfig,
    sessions: Arc<SqliteAuthSessionManager>,
    api: Arc<dyn ControlPlaneApiPort>,
) -> Result<RunningServer, ServerError> {
    let authenticator: Arc<dyn RequestAuthenticator> = sessions.clone();
    start_server_with_authenticator(config, sessions, authenticator, api, None).await
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

fn post(path: &str, body: &str, origin: &str, token: &str) -> String {
    format!(
        "POST {path} HTTP/1.1\r\nHost: control.example\r\nOrigin: {origin}\r\nAuthorization: Bearer {token}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    )
}

fn cookie_post(path: &str, body: &str, origin: &str, token: &str) -> String {
    format!(
        "POST {path} HTTP/1.1\r\nHost: control.example\r\nOrigin: {origin}\r\nCookie: wwc_session={token}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    )
}

fn cookie_get(path: &str, origin: &str, token: &str) -> String {
    format!(
        "GET {path} HTTP/1.1\r\nHost: control.example\r\nOrigin: {origin}\r\nCookie: wwc_session={token}\r\nConnection: close\r\n\r\n"
    )
}

fn cookie_delete(path: &str, body: &str, origin: &str, token: &str) -> String {
    format!(
        "DELETE {path} HTTP/1.1\r\nHost: control.example\r\nOrigin: {origin}\r\nCookie: wwc_session={token}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    )
}

fn response_json(response: &str) -> Value {
    let (_, body) = response
        .split_once("\r\n\r\n")
        .expect("HTTP response has a body boundary");
    serde_json::from_str(body).expect("HTTP response contains JSON")
}

async fn bootstrap_cookie(address: SocketAddr) -> String {
    let response = http_request(
        address,
        &post(
            "/api/v1/auth/session",
            r#"{"schemaVersion":"winwincode/v1"}"#,
            "https://client.example",
            BOOTSTRAP_PROOF,
        ),
    )
    .await;
    cookie_from_bootstrap_response(&response)
}

async fn create_session(address: SocketAddr, proof: &str) -> (String, Value) {
    let response = http_request(
        address,
        &post(
            "/api/v1/auth/session",
            r#"{"schemaVersion":"winwincode/v1"}"#,
            "https://client.example",
            proof,
        ),
    )
    .await;
    assert!(response.starts_with("HTTP/1.1 201 Created"), "{response}");
    (
        session_cookie_from_response(&response),
        response_json(&response),
    )
}

async fn current_session(address: SocketAddr, cookie: &str) -> String {
    http_request(
        address,
        &cookie_get("/api/v1/auth/session", "https://client.example", cookie),
    )
    .await
}

fn cookie_from_bootstrap_response(response: &str) -> String {
    assert!(response.starts_with("HTTP/1.1 201 Created"), "{response}");
    assert!(response.contains("cache-control: no-store"), "{response}");
    assert_eq!(
        response_json(response),
        json!({
            "schemaVersion": "winwincode/v1",
            "expiresAt": response_json(response)["expiresAt"],
            "actor": {
                "kind": "user",
                "id": "usr_00000000000000000000000001"
            },
            "authorizedScopes": [{
                "kind": "organization",
                "organizationId": "org_00000000000000000000000001"
            }]
        })
    );
    session_cookie_from_response(response)
}

fn session_cookie_from_response(response: &str) -> String {
    let set_cookie = response
        .lines()
        .find_map(|line| line.strip_prefix("set-cookie: "))
        .expect("session Set-Cookie header");
    for attribute in [
        "Path=/",
        "HttpOnly",
        "Secure",
        "SameSite=None",
        "Max-Age=",
        "Expires=",
    ] {
        assert!(set_cookie.contains(attribute), "{set_cookie}");
    }
    let pair = set_cookie.split(';').next().expect("cookie pair");
    pair.strip_prefix("wwc_session=")
        .expect("session cookie name")
        .to_owned()
}

async fn assert_websocket_session(address: SocketAddr, session_cookie: &str) {
    let mut socket = connect_websocket_session(address, session_cookie).await;
    socket
        .send(tokio_tungstenite::tungstenite::Message::Text(
            r#"{"type":"transport.subscribe.v1"}"#.into(),
        ))
        .await
        .expect("subscribe");
    let accepted = socket
        .next()
        .await
        .expect("accepted frame")
        .expect("accepted message");
    assert!(
        accepted
            .to_text()
            .expect("text")
            .contains("subscription-accepted")
    );
    let event = socket
        .next()
        .await
        .expect("event frame")
        .expect("event message");
    assert!(event.to_text().expect("text").contains("event.v1"));
    socket.close(None).await.expect("close socket");
}

async fn connect_websocket_session(
    address: SocketAddr,
    session_cookie: &str,
) -> tokio_tungstenite::WebSocketStream<TcpStream> {
    let websocket_url = format!("ws://{address}/api/v1/events");
    let mut request = websocket_url
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

async fn connect_websocket_bearer(
    address: SocketAddr,
    bearer: &str,
) -> tokio_tungstenite::WebSocketStream<TcpStream> {
    let websocket_url = format!("ws://{address}/api/v1/events");
    let mut request = websocket_url
        .into_client_request()
        .expect("WebSocket request");
    request.headers_mut().insert(
        "Authorization",
        format!("Bearer {bearer}")
            .parse()
            .expect("authorization header"),
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

fn create_enterprise_api_token(identity: &EnterpriseIdentityService) -> (ApiTokenId, String) {
    let account: EnterpriseIdentityUpdateCommand = serde_json::from_value(json!({
        "schemaVersion": "winwincode/v1",
        "command": "enterprise.identity.update",
        "actor": {
            "kind": "user",
            "id": "usr_00000000000000000000000001"
        },
        "scope": {
            "kind": "organization",
            "organizationId": "org_00000000000000000000000001"
        },
        "requestId": "req_00000000000000000000000081",
        "expectedRevision": 0,
        "payload": {
            "kind": "service_account",
            "action": "upsert",
            "serviceAccountId": "svc_00000000000000000000000001",
            "displayName": "Transport fixture",
            "authorizedScopes": [{
                "kind": "repository",
                "organizationId": "org_00000000000000000000000001",
                "workspaceId": "wsp_00000000000000000000000001",
                "projectId": "prj_00000000000000000000000001",
                "repositoryId": "rep_00000000000000000000000001"
            }]
        }
    }))
    .expect("Service Account command");
    identity.update(&account).expect("create Service Account");

    let api_token_id = ApiTokenId("tok_00000000000000000000000001".to_owned());
    let mut generated = generate_api_token(api_token_id.clone()).expect("generate API Token");
    let issue: EnterpriseIdentityUpdateCommand = serde_json::from_value(json!({
        "schemaVersion": "winwincode/v1",
        "command": "enterprise.identity.update",
        "actor": {
            "kind": "user",
            "id": "usr_00000000000000000000000001"
        },
        "scope": {
            "kind": "organization",
            "organizationId": "org_00000000000000000000000001"
        },
        "requestId": "req_00000000000000000000000082",
        "expectedRevision": 0,
        "payload": {
            "kind": "api_token",
            "action": "issue",
            "apiTokenId": api_token_id,
            "serviceAccountId": "svc_00000000000000000000000001",
            "tokenSha256": generated.token_sha256(),
            "expiresAt": "2030-01-01T00:00:00.000Z"
        }
    }))
    .expect("API Token issue command");
    identity.update(&issue).expect("issue API Token");
    (
        generated.api_token_id().clone(),
        generated.take_raw().expect("raw Token is returned once"),
    )
}

fn revoke_enterprise_api_token(identity: &EnterpriseIdentityService, api_token_id: &ApiTokenId) {
    let revoke: EnterpriseIdentityUpdateCommand = serde_json::from_value(json!({
        "schemaVersion": "winwincode/v1",
        "command": "enterprise.identity.update",
        "actor": {
            "kind": "user",
            "id": "usr_00000000000000000000000001"
        },
        "scope": {
            "kind": "organization",
            "organizationId": "org_00000000000000000000000001"
        },
        "requestId": "req_00000000000000000000000083",
        "expectedRevision": 1,
        "payload": {
            "kind": "api_token",
            "action": "revoke",
            "apiTokenId": api_token_id
        }
    }))
    .expect("API Token revoke command");
    identity.update(&revoke).expect("revoke API Token");
}

async fn assert_logout_revokes(address: SocketAddr, session_cookie: &str) {
    let logout = http_request(
        address,
        &cookie_delete(
            "/api/v1/auth/session",
            r#"{"schemaVersion":"winwincode/v1"}"#,
            "https://client.example",
            session_cookie,
        ),
    )
    .await;
    assert!(logout.starts_with("HTTP/1.1 204 No Content"), "{logout}");
    assert!(
        logout.contains(
            "set-cookie: wwc_session=; Path=/; HttpOnly; Secure; SameSite=None; Max-Age=0;"
        )
    );
    assert!(logout.contains("cache-control: no-store"), "{logout}");
    let current = http_request(
        address,
        &cookie_get(
            "/api/v1/auth/session",
            "https://client.example",
            session_cookie,
        ),
    )
    .await;
    assert!(
        current.starts_with("HTTP/1.1 401 Unauthorized"),
        "{current}"
    );
    let revoked = http_request(
        address,
        &cookie_post(
            "/api/v1/queries",
            r#"{"query":"delivery.get","schemaVersion":"winwincode/v1"}"#,
            "https://client.example",
            session_cookie,
        ),
    )
    .await;
    assert!(
        revoked.starts_with("HTTP/1.1 401 Unauthorized"),
        "{revoked}"
    );
}

#[tokio::test]
async fn one_origin_serves_authenticated_commands_queries_and_events() {
    let api = Arc::new(FakeApi::default());
    let running = start_server(
        config("127.0.0.1:0".parse().expect("address")),
        auth(),
        api.clone(),
    )
    .await
    .expect("start server");
    let address = running.local_address();
    let session_cookie = bootstrap_cookie(address).await;
    let current = http_request(
        address,
        &cookie_get(
            "/api/v1/auth/session",
            "https://client.example",
            &session_cookie,
        ),
    )
    .await;
    assert!(current.starts_with("HTTP/1.1 200 OK"), "{current}");
    assert!(current.contains("cache-control: no-store"), "{current}");
    assert_eq!(
        response_json(&current)["actor"]["id"],
        "usr_00000000000000000000000001"
    );

    let command = http_request(
        address,
        &cookie_post(
            "/api/v1/commands",
            r#"{"command":"delivery.create","schemaVersion":"winwincode/v1"}"#,
            "https://client.example",
            &session_cookie,
        ),
    )
    .await;
    assert!(command.starts_with("HTTP/1.1 200 OK"), "{command}");
    assert!(command.contains("access-control-allow-origin: https://client.example"));
    assert!(command.contains("access-control-allow-credentials: true"));

    let query = http_request(
        address,
        &cookie_post(
            "/api/v1/queries",
            r#"{"query":"delivery.get","schemaVersion":"winwincode/v1"}"#,
            "https://client.example",
            &session_cookie,
        ),
    )
    .await;
    assert!(query.starts_with("HTTP/1.1 200 OK"), "{query}");

    assert_websocket_session(address, &session_cookie).await;

    assert_eq!(api.commands.lock().expect("commands").len(), 1);
    assert_eq!(api.queries.lock().expect("queries").len(), 1);
    assert_eq!(api.subscriptions.lock().expect("subscriptions").len(), 1);
    assert_logout_revokes(address, &session_cookie).await;
    running.shutdown().await.expect("shutdown");
    assert_eq!(api.shutdowns.load(Ordering::Relaxed), 1);
}

#[tokio::test]
async fn session_context_isolates_users_shrinks_scope_revokes_and_survives_restart() {
    let directory = test_directory("winwincode-auth-context-test");
    let manager = Arc::new(
        SqliteAuthSessionManager::open(
            &directory,
            vec![
                fixture_bootstrap(
                    "proof-user-one",
                    1,
                    vec![fixture_scope(1), fixture_scope(2)],
                ),
                fixture_bootstrap("proof-user-two", 2, vec![fixture_scope(2)]),
            ],
            AuthSessionConfig::default(),
        )
        .expect("multi-user authenticator"),
    );
    let first_server = start_server(
        config("127.0.0.1:0".parse().expect("address")),
        Arc::clone(&manager),
        Arc::new(FakeApi::default()),
    )
    .await
    .expect("start first server");
    let address = first_server.local_address();
    let (first_cookie, first) = create_session(address, "proof-user-one").await;
    let (second_cookie, second) = create_session(address, "proof-user-two").await;
    assert_eq!(first["actor"]["id"], "usr_00000000000000000000000001");
    assert_eq!(first["authorizedScopes"].as_array().map(Vec::len), Some(2));
    assert_eq!(second["actor"]["id"], "usr_00000000000000000000000002");

    assert_eq!(
        manager
            .replace_authorized_scopes(&fixture_actor(1), vec![fixture_scope(2)])
            .expect("shrink first user"),
        1
    );
    let first = current_session(address, &first_cookie).await;
    let second = current_session(address, &second_cookie).await;
    let second_context = response_json(&second);
    assert_eq!(
        response_json(&first)["authorizedScopes"],
        json!([fixture_scope(2)])
    );
    assert_eq!(
        second_context["actor"]["id"],
        "usr_00000000000000000000000002"
    );
    assert_eq!(
        manager
            .revoke_actor_sessions(&fixture_actor(1))
            .expect("revoke first user"),
        1
    );
    assert!(
        current_session(address, &first_cookie)
            .await
            .starts_with("HTTP/1.1 401 Unauthorized")
    );
    first_server.shutdown().await.expect("first shutdown");
    drop(manager);

    let restarted_manager = Arc::new(
        SqliteAuthSessionManager::open(
            &directory,
            vec![fixture_bootstrap(
                "proof-user-two-after-restart",
                2,
                vec![fixture_scope(2)],
            )],
            AuthSessionConfig::default(),
        )
        .expect("restarted authenticator"),
    );
    let restarted = start_server(
        config("127.0.0.1:0".parse().expect("address")),
        restarted_manager,
        Arc::new(FakeApi::default()),
    )
    .await
    .expect("restart server");
    let restored = current_session(restarted.local_address(), &second_cookie).await;
    assert!(restored.starts_with("HTTP/1.1 200 OK"), "{restored}");
    assert_eq!(response_json(&restored), second_context);
    restarted.shutdown().await.expect("restarted shutdown");
    fs::remove_dir_all(directory).expect("remove auth context directory");
}

#[tokio::test]
async fn live_websocket_closes_when_its_session_authorization_changes() {
    let manager = auth();
    let running = start_server(
        config("127.0.0.1:0".parse().expect("address")),
        Arc::clone(&manager),
        Arc::new(FakeApi::default()),
    )
    .await
    .expect("start server");
    let address = running.local_address();
    let cookie = bootstrap_cookie(address).await;
    let mut socket = connect_websocket_session(address, &cookie).await;
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

    assert_eq!(
        manager
            .replace_authorized_scopes(&fixture_actor(1), Vec::new())
            .expect("revoke session authorization"),
        1
    );
    let closed = tokio::time::timeout(Duration::from_secs(2), socket.next())
        .await
        .expect("authorization close deadline")
        .expect("authorization close frame")
        .expect("valid authorization close frame");
    let tokio_tungstenite::tungstenite::Message::Close(Some(frame)) = closed else {
        panic!("authorization change must close WebSocket with a reason");
    };
    assert_eq!(u16::from(frame.code), 4403);
    running.shutdown().await.expect("shutdown");
}

#[tokio::test]
async fn live_websocket_sends_queued_revocation_before_authorization_close() {
    let manager = auth();
    let api = Arc::new(FakeApi::default());
    let running = start_server(
        config("127.0.0.1:0".parse().expect("address")),
        Arc::clone(&manager),
        api.clone(),
    )
    .await
    .expect("start server");
    let address = running.local_address();
    let cookie = bootstrap_cookie(address).await;
    let mut socket = connect_websocket_session(address, &cookie).await;
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

    assert_eq!(
        manager
            .replace_authorized_scopes(&fixture_actor(1), Vec::new())
            .expect("revoke session authorization"),
        1
    );
    api.event_senders
        .lock()
        .expect("event senders")
        .first()
        .expect("live event sender")
        .try_send(json!({
            "authorizationEpoch": 2,
            "closeCode": 4403,
            "subscriptionId": "sub_00000000000000000000000052",
            "type": "transport.authorization-revoked.v1"
        }))
        .expect("queue generated authorization revocation");

    let revoked = tokio::time::timeout(Duration::from_secs(2), socket.next())
        .await
        .expect("authorization revocation deadline")
        .expect("authorization revocation frame")
        .expect("valid authorization revocation frame");
    let tokio_tungstenite::tungstenite::Message::Text(revoked) = revoked else {
        panic!("authorization revocation must be sent before the close frame");
    };
    assert_eq!(
        serde_json::from_str::<Value>(&revoked).expect("authorization revocation JSON"),
        json!({
            "authorizationEpoch": 2,
            "closeCode": 4403,
            "subscriptionId": "sub_00000000000000000000000052",
            "type": "transport.authorization-revoked.v1"
        })
    );
    let closed = socket
        .next()
        .await
        .expect("authorization close frame")
        .expect("valid authorization close frame");
    let tokio_tungstenite::tungstenite::Message::Close(Some(frame)) = closed else {
        panic!("authorization revocation must close WebSocket with a reason");
    };
    assert_eq!(u16::from(frame.code), 4403);
    running.shutdown().await.expect("shutdown");
}

#[tokio::test]
async fn enterprise_api_token_authenticates_http_and_websocket_until_revoked() {
    let identity_directory = test_directory("winwincode-identity-transport-test");
    let identity = Arc::new(EnterpriseIdentityService::new(Box::new(
        SqliteStorage::open(&identity_directory).expect("identity storage"),
    )));
    let (api_token_id, raw_token) = create_enterprise_api_token(&identity);
    let authenticated = identity
        .authenticate_bearer(&raw_token)
        .expect("authenticate current API Token");
    let principal =
        AuthenticatedPrincipal::new(authenticated.actor, authenticated.authorized_scopes)
            .expect("authenticated principal");
    assert_eq!(principal.subject(), "svc_00000000000000000000000001");
    assert!(!principal.authorizes(&fixture_scope(9)));

    let sessions = auth();
    let authenticator: Arc<dyn RequestAuthenticator> = Arc::new(
        EnterpriseRequestAuthenticator::new(Arc::clone(&sessions), Arc::clone(&identity)),
    );
    let api = Arc::new(FakeApi::default());
    let running = start_server_with_authenticator(
        config("127.0.0.1:0".parse().expect("address")),
        Arc::clone(&sessions),
        authenticator,
        api.clone(),
        None,
    )
    .await
    .expect("start enterprise-authenticated server");
    let address = running.local_address();

    let query = http_request(
        address,
        &post(
            "/api/v1/queries",
            r#"{"schemaVersion":"winwincode/v1","query":"settings.get"}"#,
            "https://client.example",
            &raw_token,
        ),
    )
    .await;
    assert!(query.starts_with("HTTP/1.1 200 OK"), "{query}");
    assert!(!query.contains(&raw_token));
    assert_eq!(
        api.queries.lock().expect("queries")[0].0,
        "svc_00000000000000000000000001"
    );

    let mut socket = connect_websocket_bearer(address, &raw_token).await;
    socket
        .send(tokio_tungstenite::tungstenite::Message::Text(
            r#"{"type":"transport.subscribe.v1"}"#.into(),
        ))
        .await
        .expect("subscribe with API Token");
    let accepted = socket
        .next()
        .await
        .expect("accepted")
        .expect("accepted frame");
    assert!(
        accepted
            .to_text()
            .expect("text")
            .contains("subscription-accepted")
    );
    socket.next().await.expect("event").expect("event frame");

    revoke_enterprise_api_token(&identity, &api_token_id);
    let rejected = http_request(
        address,
        &post(
            "/api/v1/queries",
            r#"{"schemaVersion":"winwincode/v1","query":"settings.get"}"#,
            "https://client.example",
            &raw_token,
        ),
    )
    .await;
    assert!(
        rejected.starts_with("HTTP/1.1 401 Unauthorized"),
        "{rejected}"
    );
    assert!(!rejected.contains(&raw_token));
    assert_eq!(api.queries.lock().expect("queries").len(), 1);

    let closed = tokio::time::timeout(Duration::from_secs(2), socket.next())
        .await
        .expect("revoked Token close deadline")
        .expect("revoked Token close frame")
        .expect("valid revoked Token close frame");
    let tokio_tungstenite::tungstenite::Message::Close(Some(frame)) = closed else {
        panic!("revoked API Token must close WebSocket with a reason");
    };
    assert_eq!(u16::from(frame.code), 4403);

    running.shutdown().await.expect("shutdown");
    drop(identity);
    drop(sessions);
    fs::remove_dir_all(identity_directory).expect("remove identity directory");
}

#[tokio::test]
async fn session_cookie_preflight_and_version_negotiation_are_explicit() {
    let api = Arc::new(FakeApi::default());
    let running = start_server(
        config("127.0.0.1:0".parse().expect("address")),
        auth(),
        api.clone(),
    )
    .await
    .expect("start server");
    let address = running.local_address();
    let session_cookie = bootstrap_cookie(address).await;

    let cookie = http_request(
        address,
        &cookie_post(
            "/api/v1/queries",
            r#"{"query":"settings.get","schemaVersion":"winwincode/v1"}"#,
            "https://client.example",
            &session_cookie,
        ),
    )
    .await;
    assert!(cookie.starts_with("HTTP/1.1 200 OK"), "{cookie}");
    assert!(cookie.contains("access-control-allow-origin: https://client.example"));
    assert!(cookie.contains("access-control-allow-credentials: true"));

    let preflight = http_request(
        address,
        "OPTIONS /api/v1/queries HTTP/1.1\r\nHost: control.example\r\nOrigin: https://client.example\r\nAccess-Control-Request-Method: POST\r\nAccess-Control-Request-Headers: Content-Type\r\nConnection: close\r\n\r\n",
    )
    .await;
    assert!(
        preflight.starts_with("HTTP/1.1 204 No Content"),
        "{preflight}"
    );
    assert!(preflight.contains("access-control-allow-origin: https://client.example"));
    assert!(preflight.contains("access-control-allow-credentials: true"));

    let old_client = http_request(
        address,
        &cookie_post(
            "/api/v1/commands",
            r#"{"command":"delivery.create","schemaVersion":"winwincode/v0","requestId":"req_00000000000000000000000001"}"#,
            "https://client.example",
            &session_cookie,
        ),
    )
    .await;
    assert!(
        old_client.starts_with("HTTP/1.1 426 Upgrade Required"),
        "{old_client}"
    );
    assert!(old_client.contains("CLIENT_UPGRADE_REQUIRED"));
    let old_client_error: ErrorEnvelope =
        serde_json::from_value(response_json(&old_client)).expect("canonical error envelope");
    assert_eq!(
        old_client_error.request_id.0,
        "req_00000000000000000000000001"
    );
    let Error::TerminalError(error) = old_client_error.error else {
        panic!("version mismatch is terminal");
    };
    assert_eq!(error.code, TerminalErrorCode::InvalidRequest);
    assert!(!error.retryable);
    assert_eq!(
        error.details.get("reason"),
        Some(&ErrorDetailValue::Variant4(
            "CLIENT_UPGRADE_REQUIRED".to_owned()
        ))
    );
    assert!(api.commands.lock().expect("commands").is_empty());
    assert_eq!(api.queries.lock().expect("queries").len(), 1);

    let health = http_request(
        address,
        "GET /health HTTP/1.1\r\nHost: control.example\r\nConnection: close\r\n\r\n",
    )
    .await;
    assert!(health.contains(r#""schemaVersion":"winwincode/v1""#));
    assert!(health.contains(&format!(
        r#""serverVersion":"{}""#,
        env!("CARGO_PKG_VERSION")
    )));
    assert!(!health.contains("workerAddress"));
    assert!(!health.contains("providerAddress"));

    running.shutdown().await.expect("shutdown");
}

#[tokio::test]
async fn generated_error_contract_correlates_and_classifies_http_failures() {
    let api = Arc::new(FakeApi::default());
    let running = start_server(config("127.0.0.1:0".parse().expect("address")), auth(), api)
        .await
        .expect("start server");
    let address = running.local_address();
    let session_cookie = bootstrap_cookie(address).await;

    let denied = http_request(
        address,
        &cookie_post(
            "/api/v1/queries",
            r#"{"schemaVersion":"winwincode/v1","requestId":"req_00000000000000000000000002"}"#,
            "https://client.example",
            "wrong-token",
        ),
    )
    .await;
    assert!(denied.starts_with("HTTP/1.1 401 Unauthorized"), "{denied}");
    let denied: ErrorEnvelope =
        serde_json::from_value(response_json(&denied)).expect("canonical HTTP error envelope");
    assert_eq!(denied.request_id.0, "req_00000000000000000000000002");
    let Error::TerminalError(error) = denied.error else {
        panic!("authentication rejection is terminal");
    };
    assert_eq!(error.code, TerminalErrorCode::AuthenticationRequired);
    assert!(!error.retryable);

    let unavailable = http_request(
        address,
        &cookie_post(
            "/api/v1/queries",
            r#"{"query":"fixture.unavailable","schemaVersion":"winwincode/v1","requestId":"req_00000000000000000000000003"}"#,
            "https://client.example",
            &session_cookie,
        ),
    )
    .await;
    assert!(
        unavailable.starts_with("HTTP/1.1 503 Service Unavailable"),
        "{unavailable}"
    );
    let unavailable: ErrorEnvelope = serde_json::from_value(response_json(&unavailable))
        .expect("canonical retryable HTTP error envelope");
    assert_eq!(unavailable.request_id.0, "req_00000000000000000000000003");
    let Error::RetryableError(error) = unavailable.error else {
        panic!("service unavailability is retryable");
    };
    assert_eq!(error.code, RetryableErrorCode::ServiceUnavailable);
    assert!(error.retryable);

    let invalid_status = http_request(
        address,
        &cookie_post(
            "/api/v1/queries",
            r#"{"query":"fixture.invalid-status","schemaVersion":"winwincode/v1","requestId":"req_00000000000000000000000006"}"#,
            "https://client.example",
            &session_cookie,
        ),
    )
    .await;
    assert!(
        invalid_status.starts_with("HTTP/1.1 500 Internal Server Error"),
        "{invalid_status}"
    );
    let invalid_status: ErrorEnvelope = serde_json::from_value(response_json(&invalid_status))
        .expect("normalized application error envelope");
    assert_eq!(
        invalid_status.request_id.0,
        "req_00000000000000000000000006"
    );
    let Error::TerminalError(error) = invalid_status.error else {
        panic!("invalid application error status is terminal");
    };
    assert_eq!(error.code, TerminalErrorCode::InternalError);
    assert_eq!(
        error.message,
        "request failed at the public Control Plane boundary"
    );
    running.shutdown().await.expect("shutdown");
}

#[tokio::test]
async fn unknown_application_errors_do_not_expose_internal_messages() {
    let api = Arc::new(FakeApi::default());
    let running = start_server(config("127.0.0.1:0".parse().expect("address")), auth(), api)
        .await
        .expect("start server");
    let address = running.local_address();
    let session_cookie = bootstrap_cookie(address).await;
    let internal_message = http_request(
        address,
        &cookie_post(
            "/api/v1/queries",
            r#"{"query":"fixture.internal-message","schemaVersion":"winwincode/v1","requestId":"req_00000000000000000000000007"}"#,
            "https://client.example",
            &session_cookie,
        ),
    )
    .await;
    let internal_message: ErrorEnvelope =
        serde_json::from_value(response_json(&internal_message)).expect("redacted error envelope");
    let Error::TerminalError(error) = internal_message.error else {
        panic!("unknown application failure is terminal");
    };
    assert_eq!(error.code, TerminalErrorCode::InternalError);
    assert_eq!(
        error.message,
        "request failed at the public Control Plane boundary"
    );
    assert_eq!(
        error.details.get("reason"),
        Some(&ErrorDetailValue::Variant4("APPLICATION_ERROR".to_owned()))
    );
    running.shutdown().await.expect("shutdown");
}

#[tokio::test]
async fn websocket_protocol_failures_use_the_generated_error_frame() {
    let api = Arc::new(FakeApi::default());
    let running = start_server(config("127.0.0.1:0".parse().expect("address")), auth(), api)
        .await
        .expect("start server");
    let address = running.local_address();
    let session_cookie = bootstrap_cookie(address).await;
    let websocket_url = format!("ws://{address}/api/v1/events");
    let mut request = websocket_url
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
    let (mut socket, response) = client_async(request, stream)
        .await
        .expect("upgrade WebSocket");
    assert_eq!(response.status(), 101);
    socket
        .send(tokio_tungstenite::tungstenite::Message::Text("{".into()))
        .await
        .expect("send malformed frame");
    let frame = socket
        .next()
        .await
        .expect("protocol error frame")
        .expect("protocol error message");
    let frame: ControlPlaneWebSocketProtocolErrorFrame =
        serde_json::from_str(frame.to_text().expect("text frame"))
            .expect("generated WebSocket protocol error frame");
    let Error::TerminalError(error) = frame.error.error else {
        panic!("invalid frame is terminal");
    };
    assert_eq!(error.code, TerminalErrorCode::InvalidRequest);
    assert!(!error.retryable);
    running.shutdown().await.expect("shutdown");
}

#[tokio::test]
async fn websocket_reconnect_forwards_the_exact_resume_frame() {
    let api = Arc::new(FakeApi::default());
    let running = start_server(
        config("127.0.0.1:0".parse().expect("address")),
        auth(),
        api.clone(),
    )
    .await
    .expect("start server");
    let address = running.local_address();
    let session_cookie = bootstrap_cookie(address).await;

    for frame in [
        json!({ "type": "transport.subscribe.v1", "subscriptionId": "sub_fixture" }),
        json!({
            "type": "transport.resume.v1",
            "subscriptionId": "sub_fixture",
            "after": { "sequence": 1 }
        }),
    ] {
        let websocket_url = format!("ws://{address}/api/v1/events");
        let mut request = websocket_url
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
        let (mut socket, response) = client_async(request, stream)
            .await
            .expect("upgrade WebSocket");
        assert_eq!(response.status(), 101);
        socket
            .send(tokio_tungstenite::tungstenite::Message::Text(
                serde_json::to_string(&frame).expect("frame JSON").into(),
            ))
            .await
            .expect("send subscription frame");
        let _accepted = socket.next().await.expect("accepted frame");
        socket.close(None).await.expect("close socket");
    }

    assert_eq!(
        *api.subscriptions.lock().expect("subscriptions"),
        vec![
            json!({ "type": "transport.subscribe.v1", "subscriptionId": "sub_fixture" }),
            json!({
                "type": "transport.resume.v1",
                "subscriptionId": "sub_fixture",
                "after": { "sequence": 1 }
            }),
        ]
    );
    running.shutdown().await.expect("shutdown");
}

#[tokio::test]
async fn credentials_queries_origins_and_internal_routes_are_closed() {
    let api = Arc::new(FakeApi::default());
    let running = start_server(
        config("127.0.0.1:0".parse().expect("address")),
        auth(),
        api.clone(),
    )
    .await
    .expect("start server");
    let address = running.local_address();
    for request in [
        post(
            "/api/v1/commands?token=not-a-credential",
            "{}",
            "https://client.example",
            BOOTSTRAP_PROOF,
        ),
        post(
            "/api/v1/commands",
            "{}",
            "https://evil.example",
            BOOTSTRAP_PROOF,
        ),
        post(
            "/api/v1/queries",
            "{}",
            "https://client.example",
            "wrong-token",
        ),
        post(
            "/internal/workers",
            "{}",
            "https://client.example",
            BOOTSTRAP_PROOF,
        ),
        post(
            "/internal/providers",
            "{}",
            "https://client.example",
            BOOTSTRAP_PROOF,
        ),
    ] {
        let response = http_request(address, &request).await;
        assert!(!response.starts_with("HTTP/1.1 200 OK"), "{response}");
    }
    assert!(api.commands.lock().expect("commands").is_empty());
    assert!(api.queries.lock().expect("queries").is_empty());
    running.shutdown().await.expect("shutdown");
}

#[tokio::test]
async fn http_and_websocket_outputs_fail_closed_before_credential_material_is_written() {
    let api = Arc::new(FakeApi::default());
    let running = start_server(config("127.0.0.1:0".parse().expect("address")), auth(), api)
        .await
        .expect("start server");
    let address = running.local_address();
    let session_cookie = bootstrap_cookie(address).await;

    for (path, body, request_id) in [
        (
            "/api/v1/commands",
            r#"{"command":"fixture.leak","schemaVersion":"winwincode/v1","requestId":"req_00000000000000000000000004"}"#,
            "req_00000000000000000000000004",
        ),
        (
            "/api/v1/queries",
            r#"{"query":"fixture.leak","schemaVersion":"winwincode/v1","requestId":"req_00000000000000000000000005"}"#,
            "req_00000000000000000000000005",
        ),
    ] {
        let response = http_request(
            address,
            &cookie_post(path, body, "https://client.example", &session_cookie),
        )
        .await;
        assert!(response.starts_with("HTTP/1.1 500 Internal Server Error"));
        assert!(response.contains("CREDENTIAL_OUTPUT_REJECTED"));
        assert!(!response.contains(FIXTURE_SECRET));
        let envelope: ErrorEnvelope = serde_json::from_value(response_json(&response))
            .expect("credential rejection remains canonical");
        assert_eq!(envelope.request_id.0, request_id);
    }

    let websocket_url = format!("ws://{address}/api/v1/events");
    let mut request = websocket_url
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
    let (mut socket, _) = client_async(request, stream)
        .await
        .expect("upgrade WebSocket");
    socket
        .send(tokio_tungstenite::tungstenite::Message::Text(
            r#"{"type":"fixture.leak"}"#.into(),
        ))
        .await
        .expect("send leak fixture subscription");
    let closed = socket.next().await;
    if let Some(Ok(message)) = closed {
        let text = message.to_string();
        assert!(text.contains("CREDENTIAL_OUTPUT_REJECTED"));
        assert!(!text.contains(FIXTURE_SECRET));
    }

    running.shutdown().await.expect("shutdown");
}

#[tokio::test]
async fn public_component_names_with_embedded_provider_prefix_are_not_rejected() {
    let api = Arc::new(FakeApi::default());
    let running = start_server(config("127.0.0.1:0".parse().expect("address")), auth(), api)
        .await
        .expect("start server");
    let address = running.local_address();
    let session_cookie = bootstrap_cookie(address).await;
    let websocket_url = format!("ws://{address}/api/v1/events");
    let mut request = websocket_url
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
    let (mut socket, _) = client_async(request, stream)
        .await
        .expect("upgrade WebSocket");
    socket
        .send(tokio_tungstenite::tungstenite::Message::Text(
            r#"{"type":"fixture.public-component"}"#.into(),
        ))
        .await
        .expect("send public component fixture subscription");
    let frame = socket
        .next()
        .await
        .expect("public component frame")
        .expect("valid public component frame");
    let value: Value =
        serde_json::from_str(frame.to_text().expect("text frame")).expect("public component JSON");
    assert_eq!(
        value["source"]["component"],
        "delivery-task-breakdown-transaction"
    );
    assert_ne!(value["type"], "transport.error.v1");
    socket.close(None).await.expect("close socket");
    running.shutdown().await.expect("shutdown");
}

#[tokio::test]
async fn graceful_shutdown_releases_the_listener_for_restart() {
    let address = {
        let first_api = Arc::new(FakeApi::default());
        let first = start_server(
            config("127.0.0.1:0".parse().expect("address")),
            auth(),
            first_api.clone(),
        )
        .await
        .expect("start first server");
        let address = first.local_address();
        first.shutdown().await.expect("stop first server");
        assert_eq!(first_api.shutdowns.load(Ordering::Relaxed), 1);
        address
    };
    let second_api = Arc::new(FakeApi::default());
    let second = start_server(config(address), auth(), second_api.clone())
        .await
        .expect("restart server on released address");
    assert_eq!(second.local_address(), address);
    second.shutdown().await.expect("stop second server");
    assert_eq!(second_api.shutdowns.load(Ordering::Relaxed), 1);
}

async fn assert_https_query(
    connector: &TlsConnector,
    server_name: &ServerName<'static>,
    address: SocketAddr,
    session_cookie: &str,
) {
    let tcp = TcpStream::connect(address).await.expect("connect HTTPS");
    let mut tls = connector
        .connect(server_name.clone(), tcp)
        .await
        .expect("HTTPS handshake");
    let request = cookie_post(
        "/api/v1/queries",
        r#"{"query":"settings.get","schemaVersion":"winwincode/v1"}"#,
        "https://client.example",
        session_cookie,
    );
    tls.write_all(request.as_bytes())
        .await
        .expect("write HTTPS request");
    let mut response = Vec::new();
    tls.read_to_end(&mut response)
        .await
        .expect("read HTTPS response");
    let response = String::from_utf8(response).expect("HTTPS response");
    assert!(response.starts_with("HTTP/1.1 200 OK"), "{response}");
    assert!(response.contains("access-control-allow-credentials: true"));
}

async fn assert_wss_subscription(
    connector: &TlsConnector,
    server_name: ServerName<'static>,
    address: SocketAddr,
    session_cookie: &str,
) {
    let tcp = TcpStream::connect(address).await.expect("connect WSS");
    let tls = connector
        .connect(server_name, tcp)
        .await
        .expect("WSS TLS handshake");
    let mut request = format!("wss://localhost:{}/api/v1/events", address.port())
        .into_client_request()
        .expect("WSS request");
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
    let (mut socket, response) = client_async(request, tls).await.expect("upgrade WSS");
    assert_eq!(response.status(), 101);
    assert_eq!(
        response
            .headers()
            .get("access-control-allow-origin")
            .expect("allowed WSS origin"),
        "https://client.example"
    );
    socket
        .send(tokio_tungstenite::tungstenite::Message::Text(
            r#"{"type":"transport.subscribe.v1"}"#.into(),
        ))
        .await
        .expect("subscribe over WSS");
    let accepted = socket
        .next()
        .await
        .expect("accepted WSS frame")
        .expect("accepted WSS message");
    assert!(
        accepted
            .to_text()
            .expect("text")
            .contains("subscription-accepted")
    );
    socket.close(None).await.expect("close WSS");
}

async fn bootstrap_cookie_https(
    connector: &TlsConnector,
    server_name: &ServerName<'static>,
    address: SocketAddr,
) -> String {
    let tcp = TcpStream::connect(address).await.expect("connect HTTPS");
    let mut tls = connector
        .connect(server_name.clone(), tcp)
        .await
        .expect("HTTPS handshake");
    let request = post(
        "/api/v1/auth/session",
        r#"{"schemaVersion":"winwincode/v1"}"#,
        "https://client.example",
        BOOTSTRAP_PROOF,
    );
    tls.write_all(request.as_bytes())
        .await
        .expect("write auth request");
    let mut response = Vec::new();
    tls.read_to_end(&mut response)
        .await
        .expect("read auth response");
    cookie_from_bootstrap_response(&String::from_utf8(response).expect("HTTPS response"))
}

#[tokio::test]
async fn separate_origin_uses_https_and_wss_with_the_same_session() {
    let _ = rustls::crypto::aws_lc_rs::default_provider().install_default();
    let directory = test_directory("winwincode-server-tls-test");
    fs::create_dir_all(&directory).expect("create TLS test directory");
    let certificate_path = directory.join("certificate.pem");
    let private_key_path = directory.join("private-key.pem");
    let CertifiedKey { cert, signing_key } =
        generate_simple_self_signed(vec!["localhost".to_owned()]).expect("generate certificate");
    fs::write(&certificate_path, cert.pem()).expect("write certificate");
    fs::write(&private_key_path, signing_key.serialize_pem()).expect("write private key");

    let server_config = ServerConfig::new(
        "127.0.0.1:0".parse().expect("address"),
        "https://control.example",
        ServerTls::Pem {
            certificate_path,
            private_key_path,
        },
        BTreeSet::from(["https://client.example".to_owned()]),
        directory.join("data"),
        Duration::from_secs(2),
    )
    .expect("TLS server configuration");
    let api = Arc::new(FakeApi::default());
    let running = start_server(server_config, auth(), api.clone())
        .await
        .expect("start TLS server");
    let address = running.local_address();

    let mut roots = RootCertStore::empty();
    roots
        .add(cert.der().clone())
        .expect("trust test certificate");
    let provider = rustls::crypto::aws_lc_rs::default_provider();
    let client_config = ClientConfig::builder_with_provider(Arc::new(provider))
        .with_safe_default_protocol_versions()
        .expect("safe TLS protocol versions")
        .with_root_certificates(roots)
        .with_no_client_auth();
    let connector = TlsConnector::from(Arc::new(client_config));
    let server_name = ServerName::try_from("localhost")
        .expect("server name")
        .to_owned();

    let session_cookie = bootstrap_cookie_https(&connector, &server_name, address).await;
    assert_https_query(&connector, &server_name, address, &session_cookie).await;
    assert_wss_subscription(&connector, server_name, address, &session_cookie).await;

    assert_eq!(api.queries.lock().expect("queries").len(), 1);
    assert_eq!(api.subscriptions.lock().expect("subscriptions").len(), 1);
    running.shutdown().await.expect("shutdown TLS server");
    fs::remove_dir_all(directory).expect("remove TLS test directory");
}

#[test]
fn public_url_tls_and_storage_are_explicit_and_validated() {
    let error = ServerConfig::new(
        "127.0.0.1:8443".parse().expect("address"),
        "http://control.example",
        ServerTls::Pem {
            certificate_path: PathBuf::from("cert.pem"),
            private_key_path: PathBuf::from("key.pem"),
        },
        BTreeSet::from(["https://client.example".to_owned()]),
        PathBuf::from("data"),
        Duration::from_secs(30),
    )
    .expect_err("HTTP public URL must not accompany TLS");
    assert!(error.to_string().contains("scheme"));

    for public_url in [
        "https://user:secret@control.example",
        "https://control.example/path",
        "https://control.example?token=secret",
    ] {
        assert!(
            ServerConfig::new(
                "127.0.0.1:8443".parse().expect("address"),
                public_url,
                ServerTls::Pem {
                    certificate_path: PathBuf::from("cert.pem"),
                    private_key_path: PathBuf::from("key.pem"),
                },
                BTreeSet::from(["https://client.example".to_owned()]),
                PathBuf::from("data"),
                Duration::from_secs(30),
            )
            .is_err()
        );
    }
}
