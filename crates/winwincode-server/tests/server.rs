// SPDX-License-Identifier: Apache-2.0

use std::collections::BTreeSet;
use std::fs;
use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::Duration;

use futures::{SinkExt, StreamExt};
use serde_json::{Value, json};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;
use tokio::sync::mpsc;
use tokio_tungstenite::client_async;
use tokio_tungstenite::tungstenite::client::IntoClientRequest;
use winwincode_api::generated::{
    Actor, ControlPlaneWebSocketProtocolErrorFrame, Error, ErrorDetailValue, ErrorEnvelope,
    OrganizationScope, OrganizationScopeKind, RetryableErrorCode, Scope, TerminalErrorCode,
};
use winwincode_domain::{Instant, OrganizationId, UserActor, UserActorKind, UserId};
use winwincode_server::{
    ApiError, AuthSessionBootstrap, AuthSessionConfig, AuthenticatedPrincipal, ControlPlaneApiPort,
    EventSubscription, RequestAuthenticator, RunningServer, ServerConfig, ServerError,
    SqliteAuthSessionManager, UserAccountService, start_server as start_server_with_authenticator,
};

const FIXTURE_SECRET: &str = "sk-fixturecredentialleakgate1234567890";
const BOOTSTRAP_PROOF: &str = "test-bootstrap-proof";
const OWNER_USERNAME: &str = "owner";
const OWNER_PASSWORD: &str = "initial-owner-password";

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

fn actor_for(user_id: &str) -> Actor {
    Actor::UserActor(UserActor {
        kind: UserActorKind::User,
        id: UserId(user_id.to_owned()),
    })
}

fn fixture_scope(seed: u8) -> Scope {
    Scope::OrganizationScope(OrganizationScope {
        kind: OrganizationScopeKind::Organization,
        organization_id: OrganizationId(format!("org_{seed:026}")),
    })
}

fn open_auth(
    directory: &std::path::Path,
    proof: &str,
    scopes: Vec<Scope>,
) -> (Arc<UserAccountService>, Arc<SqliteAuthSessionManager>) {
    let accounts = Arc::new(UserAccountService::open(directory).expect("account service"));
    let sessions = Arc::new(
        SqliteAuthSessionManager::open(
            directory.join("auth-sessions"),
            vec![AuthSessionBootstrap::new(proof).expect("proof")],
            scopes,
            AuthSessionConfig::default(),
            Arc::clone(&accounts),
            None,
        )
        .expect("session manager"),
    );
    (accounts, sessions)
}

#[derive(Default)]
struct BlockingCall {
    entered: tokio::sync::Notify,
}

impl BlockingCall {
    fn pause(&self) {
        self.entered.notify_one();
        std::thread::sleep(Duration::from_secs(2));
    }
}

#[derive(Default)]
struct FakeApi {
    commands: Mutex<Vec<(String, Value)>>,
    queries: Mutex<Vec<(String, Value)>>,
    subscriptions: Mutex<Vec<Value>>,
    event_senders: Mutex<Vec<mpsc::Sender<Value>>>,
    shutdowns: AtomicU64,
    blocking: Arc<BlockingCall>,
}

impl ControlPlaneApiPort for FakeApi {
    fn command(
        &self,
        principal: &AuthenticatedPrincipal,
        request: Value,
    ) -> Result<Value, ApiError> {
        assert_ne!(
            request.get("command").and_then(Value::as_str),
            Some("fixture.panic"),
            "private fixture panic {FIXTURE_SECRET}"
        );
        if request.get("command").and_then(Value::as_str) == Some("fixture.slow") {
            self.blocking.pause();
        }
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
        assert_ne!(
            request.get("query").and_then(Value::as_str),
            Some("fixture.panic"),
            "private fixture panic {FIXTURE_SECRET}"
        );
        if request.get("query").and_then(Value::as_str) == Some("fixture.slow") {
            self.blocking.pause();
        }
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
        BTreeSet::from(["https://client.example".to_owned()]),
        test_directory("winwincode-server-test"),
        Duration::from_secs(2),
    )
    .expect("valid config")
}

fn cleanup_recovery_config() -> ServerConfig {
    config("127.0.0.1:0".parse().unwrap())
}

fn auth() -> Arc<SqliteAuthSessionManager> {
    let directory = test_directory("winwincode-auth-test");
    let (_, sessions) = open_auth(&directory, BOOTSTRAP_PROOF, vec![fixture_scope(1)]);
    sessions
}

async fn start_server(
    config: ServerConfig,
    sessions: Arc<SqliteAuthSessionManager>,
    api: Arc<dyn ControlPlaneApiPort>,
) -> Result<RunningServer, ServerError> {
    let authenticator: Arc<dyn RequestAuthenticator> = sessions.clone();
    start_server_with_authenticator(config, sessions, authenticator, api).await
}

#[tokio::test]
async fn accepted_cleanup_retries_without_worker_requests_and_drains_before_application_close() {
    use std::sync::Condvar;
    use winwincode_server::{
        RemoteWorkerExchangePort, RemoteWorkerTransportError, start_server_with_remote_worker,
    };
    #[derive(Default)]
    struct Recovery {
        calls: AtomicU64,
        entered: tokio::sync::Notify,
        released: (Mutex<bool>, Condvar),
    }
    impl RemoteWorkerExchangePort for Recovery {
        fn recover_accepted_outcomes(&self) -> Result<usize, RemoteWorkerTransportError> {
            if self.calls.fetch_add(1, Ordering::SeqCst) == 0 {
                return Err(RemoteWorkerTransportError::unavailable());
            }
            self.entered.notify_one();
            let (state, changed) = &self.released;
            let mut released = state.lock().unwrap();
            while !*released {
                let (next, timeout) = changed
                    .wait_timeout(released, Duration::from_secs(5))
                    .unwrap();
                released = next;
                if timeout.timed_out() && !*released {
                    return Err(RemoteWorkerTransportError::unavailable());
                }
            }
            Ok(1)
        }
        fn exchange(
            &self,
            _: &str,
            _: &str,
            _: &str,
            _: &[u8],
            _: Instant,
        ) -> Result<Vec<u8>, RemoteWorkerTransportError> {
            panic!("cleanup must run without an incoming Worker request")
        }
    }
    let api = Arc::new(FakeApi::default());
    let recovery = Arc::new(Recovery::default());
    let sessions = auth();
    let running = start_server_with_remote_worker(
        cleanup_recovery_config(),
        sessions.clone(),
        sessions,
        api.clone(),
        Some(recovery.clone()),
        None,
    )
    .await
    .unwrap();
    tokio::time::timeout(Duration::from_secs(5), recovery.entered.notified())
        .await
        .unwrap();
    assert_eq!(recovery.calls.load(Ordering::SeqCst), 2);
    let shutdown = tokio::spawn(running.shutdown());
    tokio::time::sleep(Duration::from_millis(50)).await;
    assert_eq!(
        api.shutdowns.load(Ordering::SeqCst),
        0,
        "CP stays open during owned cleanup"
    );
    *recovery.released.0.lock().unwrap() = true;
    recovery.released.1.notify_all();
    tokio::time::timeout(Duration::from_secs(5), shutdown)
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    assert_eq!(api.shutdowns.load(Ordering::SeqCst), 1);
    assert_eq!(
        recovery.calls.load(Ordering::SeqCst),
        2,
        "shutdown starts no further cleanup page"
    );
}

#[tokio::test]
async fn device_worker_relay_preserves_bodies_and_wraps_worker_status() {
    use winwincode_server::{
        RemoteWorkerExchangePort, RemoteWorkerTransportError, start_server_with_remote_worker,
    };
    struct Boundary;
    impl RemoteWorkerExchangePort for Boundary {
        fn recover_accepted_outcomes(&self) -> Result<usize, RemoteWorkerTransportError> {
            Ok(0)
        }
        fn exchange(
            &self,
            node: &str,
            worker: &str,
            instance: &str,
            body: &[u8],
            _: Instant,
        ) -> Result<Vec<u8>, RemoteWorkerTransportError> {
            assert_eq!(node, "cnd_00000000000000000000000001");
            assert_eq!(worker, "wrk_00000000000000000000000001");
            assert_eq!(instance, "wki_00000000000000000000000001");
            Ok(body.to_vec())
        }
        fn accounting(
            &self,
            _: &str,
            _: &str,
            _: &str,
            body: Option<&[u8]>,
            offset: u64,
            _: Instant,
        ) -> Result<Vec<u8>, RemoteWorkerTransportError> {
            assert_eq!(offset, 17);
            body.map(<[u8]>::to_vec)
                .ok_or_else(RemoteWorkerTransportError::unavailable)
        }
    }
    let sessions = auth();
    let running = start_server_with_remote_worker(
        config("127.0.0.1:0".parse().unwrap()),
        sessions.clone(),
        sessions,
        Arc::new(FakeApi::default()),
        Some(Arc::new(Boundary)),
        None,
    )
    .await
    .unwrap();
    for (request, expected) in [
        (
            json!({"kind":"exchange","body":"{ \"wire\": [1, 2] }\n"}),
            json!({"status":200,"body":"{ \"wire\": [1, 2] }\n"}),
        ),
        (
            json!({"kind":"accounting","body":"{ \"receipt\": 1 }","offset":17}),
            json!({"status":200,"body":"{ \"receipt\": 1 }"}),
        ),
        (
            json!({"kind":"accounting","body":null,"offset":17}),
            json!({"status":503,"body":"remote Worker transport is unavailable"}),
        ),
    ] {
        let body = json!({"clientNodeId":"cnd_00000000000000000000000001","workerId":"wrk_00000000000000000000000001","workerInstanceId":"wki_00000000000000000000000001","request":request}).to_string();
        let response = http_request(
            running.local_address(),
            &plain_post(
                "/internal/v1/client/worker-exchange",
                &body,
                "https://client.example",
            ),
        )
        .await;
        assert!(response.starts_with("HTTP/1.1 200"), "{response}");
        assert_eq!(response_json(&response), expected);
    }
    running.shutdown().await.unwrap();
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

fn plain_post(path: &str, body: &str, origin: &str) -> String {
    format!(
        "POST {path} HTTP/1.1\r\nHost: control.example\r\nOrigin: {origin}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
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
    let response = initialize_owner(address, BOOTSTRAP_PROOF, OWNER_USERNAME, OWNER_PASSWORD).await;
    cookie_from_bootstrap_response(&response, false)
}

async fn initialize_owner(
    address: SocketAddr,
    proof: &str,
    username: &str,
    password: &str,
) -> String {
    http_request(
        address,
        &post(
            "/api/v1/auth/session",
            &login_body(username, password),
            "https://client.example",
            proof,
        ),
    )
    .await
}

fn login_body(username: &str, password: &str) -> String {
    json!({
        "schemaVersion": "winwincode/v1",
        "username": username,
        "password": password,
    })
    .to_string()
}

async fn login_response(address: SocketAddr, username: &str, password: &str) -> String {
    http_request(
        address,
        &plain_post(
            "/api/v1/auth/session",
            &login_body(username, password),
            "https://client.example",
        ),
    )
    .await
}

async fn login_as(address: SocketAddr, username: &str, password: &str) -> (String, Value) {
    let response = login_response(address, username, password).await;
    assert!(response.starts_with("HTTP/1.1 201 Created"), "{response}");
    (
        session_cookie_from_response(&response, false),
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

fn cookie_from_bootstrap_response(response: &str, secure: bool) -> String {
    assert!(response.starts_with("HTTP/1.1 201 Created"), "{response}");
    assert!(response.contains("cache-control: no-store"), "{response}");
    let body = response_json(response);
    assert_eq!(body["schemaVersion"], "winwincode/v1");
    assert_eq!(body["actor"]["kind"], "user");
    assert!(
        body["actor"]["id"]
            .as_str()
            .expect("actor id")
            .starts_with("usr_")
    );
    assert_eq!(body["authorizedScopes"][0]["kind"], "organization");
    session_cookie_from_response(response, secure)
}

fn session_cookie_from_response(response: &str, secure: bool) -> String {
    let set_cookie = response
        .lines()
        .find_map(|line| line.strip_prefix("set-cookie: "))
        .expect("session Set-Cookie header");
    for attribute in ["Path=/", "HttpOnly", "Max-Age=", "Expires="] {
        assert!(set_cookie.contains(attribute), "{set_cookie}");
    }
    assert_eq!(set_cookie.contains("Secure"), secure, "{set_cookie}");
    let same_site = if secure {
        "SameSite=None"
    } else {
        "SameSite=Lax"
    };
    assert!(set_cookie.contains(same_site), "{set_cookie}");
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
        logout.contains("set-cookie: wwc_session=; Path=/; HttpOnly; SameSite=Lax; Max-Age=0;")
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
    assert!(
        response_json(&current)["actor"]["id"]
            .as_str()
            .expect("actor id")
            .starts_with("usr_")
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

    let clients = http_request(
        address,
        &cookie_get("/api/v1/clients", "https://client.example", &session_cookie),
    )
    .await;
    assert!(clients.starts_with("HTTP/1.1 404 Not Found"), "{clients}");
    assert!(clients.contains("access-control-allow-origin: https://client.example"));

    assert_websocket_session(address, &session_cookie).await;

    assert_eq!(api.commands.lock().expect("commands").len(), 1);
    assert_eq!(api.queries.lock().expect("queries").len(), 1);
    assert_eq!(api.subscriptions.lock().expect("subscriptions").len(), 1);
    assert_logout_revokes(address, &session_cookie).await;
    running.shutdown().await.expect("shutdown");
    assert_eq!(api.shutdowns.load(Ordering::Relaxed), 1);
}

#[tokio::test]
async fn session_context_updates_all_owner_sessions_and_survives_restart() {
    let directory = test_directory("winwincode-auth-context-test");
    let (accounts, manager) = open_auth(
        &directory,
        "proof-owner-initialization",
        vec![fixture_scope(1), fixture_scope(2)],
    );
    let first_server = start_server(
        config("127.0.0.1:0".parse().expect("address")),
        Arc::clone(&manager),
        Arc::new(FakeApi::default()),
    )
    .await
    .expect("start first server");
    let address = first_server.local_address();
    let owner_cookie = session_cookie_from_response(
        &initialize_owner(
            address,
            "proof-owner-initialization",
            OWNER_USERNAME,
            OWNER_PASSWORD,
        )
        .await,
        false,
    );
    let (second_cookie, second) = login_as(address, OWNER_USERNAME, OWNER_PASSWORD).await;
    let owner_user_id = manager
        .initialized_owner()
        .expect("durable Owner marker")
        .0
        .clone();
    assert!(owner_user_id.starts_with("usr_"));
    assert_eq!(second["actor"]["id"], owner_user_id);
    assert_eq!(second["authorizedScopes"].as_array().map(Vec::len), Some(2));

    let owner_actor = actor_for(&owner_user_id);
    assert_eq!(
        manager
            .replace_authorized_scopes(&owner_actor, vec![fixture_scope(2)])
            .expect("shrink owner"),
        2
    );
    let first = current_session(address, &owner_cookie).await;
    let second = current_session(address, &second_cookie).await;
    let second_context = response_json(&second);
    assert_eq!(
        response_json(&first)["authorizedScopes"],
        json!([fixture_scope(2)])
    );
    assert_eq!(second_context["actor"]["id"], owner_user_id);
    first_server.shutdown().await.expect("first shutdown");
    drop(manager);
    drop(accounts);

    let (restarted_accounts, restarted_manager) =
        open_auth(&directory, "proof-after-restart", vec![fixture_scope(2)]);
    let _ = restarted_accounts;
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

    let owner_actor = actor_for(&manager.initialized_owner().expect("durable Owner marker").0);
    assert_eq!(
        manager
            .replace_authorized_scopes(&owner_actor, Vec::new())
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

    let owner_actor = actor_for(&manager.initialized_owner().expect("durable Owner marker").0);
    assert_eq!(
        manager
            .replace_authorized_scopes(&owner_actor, Vec::new())
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

#[allow(clippy::too_many_lines)]
async fn assert_blocking_handler_preserves_other_route(blocked_route: &str) {
    use winwincode_server::{
        RemoteWorkerExchangePort, RemoteWorkerTransportError, start_server_with_remote_worker,
    };
    struct Boundary(Arc<BlockingCall>);
    impl RemoteWorkerExchangePort for Boundary {
        fn recover_accepted_outcomes(&self) -> Result<usize, RemoteWorkerTransportError> {
            Ok(0)
        }
        fn exchange(
            &self,
            _: &str,
            _: &str,
            _: &str,
            body: &[u8],
            _: Instant,
        ) -> Result<Vec<u8>, RemoteWorkerTransportError> {
            if body == b"slow" {
                self.0.pause();
            }
            Ok(b"{}".to_vec())
        }
    }
    let api = Arc::new(FakeApi::default());
    let sessions = auth();
    let worker_blocking = Arc::new(BlockingCall::default());
    let running = start_server_with_remote_worker(
        config("127.0.0.1:0".parse().unwrap()),
        sessions.clone(),
        sessions,
        api.clone(),
        Some(Arc::new(Boundary(worker_blocking.clone()))),
        None,
    )
    .await
    .unwrap();
    let address = running.local_address();
    let cookie = bootstrap_cookie(address).await;
    let relay = |body: &str| {
        let body = json!({"clientNodeId":"cnd_00000000000000000000000001","workerId":"wrk_00000000000000000000000001","workerInstanceId":"wki_00000000000000000000000001","request":{"kind":"exchange","body":body}}).to_string();
        format!(
            "POST /internal/v1/client/worker-exchange HTTP/1.1\r\nHost: control.example\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
            body.len()
        )
    };
    let worker_is_slow = blocked_route == "/internal/v1/client/worker-exchange";
    let slow_request = if worker_is_slow {
        relay("slow")
    } else {
        let field = if blocked_route.ends_with("queries") {
            "query"
        } else {
            "command"
        };
        cookie_post(blocked_route, &json!({"requestId":"req_00000000000000000000000001","schemaVersion":"winwincode/v1",field:"fixture.slow"}).to_string(), "https://client.example", &cookie)
    };
    let request = tokio::spawn(async move { http_request(address, &slow_request).await });
    if worker_is_slow {
        worker_blocking.entered.notified().await;
    } else {
        api.blocking.entered.notified().await;
    }
    let started = std::time::Instant::now();
    let response = if worker_is_slow {
        http_request(address, &cookie_post("/api/v1/queries", &json!({"requestId":"req_00000000000000000000000002","schemaVersion":"winwincode/v1","query":"fixture.fast"}).to_string(), "https://client.example", &cookie)).await
    } else {
        http_request(address, &relay("fast")).await
    };
    assert!(response.starts_with("HTTP/1.1 200"), "{response}");
    assert!(started.elapsed() < Duration::from_millis(1500));
    assert!(request.await.unwrap().starts_with("HTTP/1.1 200"));
    running.shutdown().await.unwrap();
}

#[tokio::test]
async fn slow_control_query_keeps_worker_exchange_responsive() {
    assert_blocking_handler_preserves_other_route("/api/v1/queries").await;
}

#[tokio::test]
async fn slow_control_command_keeps_worker_exchange_responsive() {
    assert_blocking_handler_preserves_other_route("/api/v1/commands").await;
}

#[tokio::test]
async fn slow_worker_exchange_keeps_control_query_responsive() {
    assert_blocking_handler_preserves_other_route("/internal/v1/client/worker-exchange").await;
}

#[tokio::test]
async fn blocking_api_panic_keeps_request_identity_and_returns_safe_unavailable() {
    let api = Arc::new(FakeApi::default());
    let running = start_server(config("127.0.0.1:0".parse().unwrap()), auth(), api.clone())
        .await
        .unwrap();
    let address = running.local_address();
    let cookie = bootstrap_cookie(address).await;
    for (path, kind) in [
        ("/api/v1/commands", "command"),
        ("/api/v1/queries", "query"),
    ] {
        let body = json!({
            "schemaVersion": "winwincode/v1",
            "requestId": "req_00000000000000000000000001",
            kind: "fixture.panic",
        })
        .to_string();
        let response = http_request(
            address,
            &cookie_post(path, &body, "https://client.example", &cookie),
        )
        .await;
        assert!(response.starts_with("HTTP/1.1 503 Service Unavailable"));
        assert!(response.contains("access-control-allow-origin: https://client.example"));
        assert!(!response.contains(FIXTURE_SECRET));
        assert!(!response.contains("private fixture panic"));
        let envelope: ErrorEnvelope = serde_json::from_value(response_json(&response)).unwrap();
        assert_eq!(envelope.request_id.0, "req_00000000000000000000000001");
        let Error::RetryableError(error) = envelope.error else {
            panic!("blocking task failure must remain retryable");
        };
        assert_eq!(error.code, RetryableErrorCode::ServiceUnavailable);
        assert!(error.retryable);
    }
    assert!(api.commands.lock().unwrap().is_empty());
    assert!(api.queries.lock().unwrap().is_empty());
    running.shutdown().await.unwrap();
}

#[test]
fn server_config_validates_origins_and_storage() {
    assert!(
        ServerConfig::new(
            "127.0.0.1:0".parse().unwrap(),
            "invalid",
            ["http://localhost:8080".to_owned()],
            PathBuf::from("data"),
            Duration::from_secs(1)
        )
        .is_err()
    );
    assert!(
        ServerConfig::new(
            "127.0.0.1:0".parse().unwrap(),
            "http://127.0.0.1:8443",
            ["http://localhost:8080".to_owned()],
            PathBuf::new(),
            Duration::from_secs(1)
        )
        .is_err()
    );
}

fn origin_get(path: &str, origin: &str) -> String {
    format!(
        "GET {path} HTTP/1.1\r\nHost: control.example\r\nOrigin: {origin}\r\nConnection: close\r\n\r\n"
    )
}

#[tokio::test]
async fn first_initialization_completes_once_and_restarts_stay_closed() {
    let directory = test_directory("winwincode-initialization-gate-test");
    let (accounts, manager) = open_auth(&directory, "proof-initialization", vec![fixture_scope(1)]);
    let api = Arc::new(FakeApi::default());
    let running = start_server(
        config("127.0.0.1:0".parse().expect("address")),
        Arc::clone(&manager),
        api,
    )
    .await
    .expect("start server");
    let address = running.local_address();

    let wrong_proof =
        initialize_owner(address, "wrong-proof", "third", "third-owner-password").await;
    assert!(
        wrong_proof.starts_with("HTTP/1.1 401 Unauthorized"),
        "{wrong_proof}"
    );
    let _owner_cookie = session_cookie_from_response(
        &initialize_owner(
            address,
            "proof-initialization",
            OWNER_USERNAME,
            OWNER_PASSWORD,
        )
        .await,
        false,
    );
    assert!(manager.initialized_owner().is_some());
    drop(accounts);

    let second = initialize_owner(
        address,
        "proof-initialization",
        "second",
        "second-owner-password",
    )
    .await;
    assert!(second.starts_with("HTTP/1.1 409 Conflict"), "{second}");
    assert!(second.contains("WRONG_STATE"), "{second}");
    let wrong_proof_again =
        initialize_owner(address, "wrong-proof", "third", "third-owner-password").await;
    assert!(
        wrong_proof_again.starts_with("HTTP/1.1 409 Conflict"),
        "{wrong_proof_again}"
    );
    running.shutdown().await.expect("shutdown");
    drop(manager);

    let (restarted_accounts, restarted_manager) =
        open_auth(&directory, "proof-after-restart", vec![fixture_scope(1)]);
    assert!(restarted_manager.initialized_owner().is_some());
    let restarted = start_server(
        config("127.0.0.1:0".parse().expect("address")),
        restarted_manager,
        Arc::new(FakeApi::default()),
    )
    .await
    .expect("restart server");
    let after_restart = initialize_owner(
        restarted.local_address(),
        "proof-after-restart",
        "fourth",
        "fourth-owner-password",
    )
    .await;
    assert!(
        after_restart.starts_with("HTTP/1.1 409 Conflict"),
        "{after_restart}"
    );
    let (cookie, _) = login_as(restarted.local_address(), OWNER_USERNAME, OWNER_PASSWORD).await;
    assert!(
        current_session(restarted.local_address(), &cookie)
            .await
            .starts_with("HTTP/1.1 200 OK")
    );
    drop(restarted_accounts);
    restarted.shutdown().await.expect("restarted shutdown");
    fs::remove_dir_all(directory).expect("remove initialization directory");
}

#[tokio::test]
async fn login_rate_limiting_returns_an_explicit_error() {
    let running = start_server(
        config("127.0.0.1:0".parse().expect("address")),
        auth(),
        Arc::new(FakeApi::default()),
    )
    .await
    .expect("start server");
    let address = running.local_address();
    let _owner_cookie = bootstrap_cookie(address).await;

    for attempt in 0..5 {
        let failed = login_response(address, "owner", "wrong-password-1").await;
        assert!(
            failed.starts_with("HTTP/1.1 401 Unauthorized"),
            "attempt {attempt}: {failed}"
        );
    }
    let limited = login_response(address, "owner", OWNER_PASSWORD).await;
    assert!(
        limited.starts_with("HTTP/1.1 429 Too Many Requests"),
        "{limited}"
    );
    assert!(limited.contains("RATE_LIMITED"), "{limited}");

    let unknown = login_response(address, "ada", OWNER_PASSWORD).await;
    assert!(
        unknown.starts_with("HTTP/1.1 401 Unauthorized"),
        "{unknown}"
    );
    running.shutdown().await.expect("shutdown");
}

#[tokio::test]
async fn owner_password_change_rotates_the_only_credential() {
    let running = start_server(
        config("127.0.0.1:0".parse().expect("address")),
        auth(),
        Arc::new(FakeApi::default()),
    )
    .await
    .expect("start server");
    let address = running.local_address();
    let owner_cookie = bootstrap_cookie(address).await;

    let removed_users_route = http_request(
        address,
        &cookie_post(
            "/api/v1/users",
            r#"{"schemaVersion":"winwincode/v1","username":"ada","role":"member"}"#,
            "https://client.example",
            &owner_cookie,
        ),
    )
    .await;
    assert!(
        removed_users_route.starts_with("HTTP/1.1 404 Not Found"),
        "{removed_users_route}"
    );

    let wrong_current = http_request(
        address,
        &cookie_post(
            "/api/v1/auth/password",
            r#"{"schemaVersion":"winwincode/v1","currentPassword":"wrong-owner-password","newPassword":"rotated-owner-password"}"#,
            "https://client.example",
            &owner_cookie,
        ),
    )
    .await;
    assert!(
        wrong_current.starts_with("HTTP/1.1 401 Unauthorized"),
        "{wrong_current}"
    );

    let changed = http_request(
        address,
        &cookie_post(
            "/api/v1/auth/password",
            r#"{"schemaVersion":"winwincode/v1","currentPassword":"initial-owner-password","newPassword":"rotated-owner-password"}"#,
            "https://client.example",
            &owner_cookie,
        ),
    )
    .await;
    assert!(changed.starts_with("HTTP/1.1 204 No Content"), "{changed}");

    let old_login = login_response(address, OWNER_USERNAME, OWNER_PASSWORD).await;
    assert!(
        old_login.starts_with("HTTP/1.1 401 Unauthorized"),
        "{old_login}"
    );
    let (_, rotated) = login_as(address, OWNER_USERNAME, "rotated-owner-password").await;
    assert_eq!(rotated["actor"]["kind"], "user");
    running.shutdown().await.expect("shutdown");
}

#[tokio::test]
async fn unauthenticated_requests_are_rejected() {
    let running = start_server(
        config("127.0.0.1:0".parse().expect("address")),
        auth(),
        Arc::new(FakeApi::default()),
    )
    .await
    .expect("start server");
    let address = running.local_address();

    let no_cookie = http_request(
        address,
        &origin_get("/api/v1/auth/session", "https://client.example"),
    )
    .await;
    assert!(
        no_cookie.starts_with("HTTP/1.1 401 Unauthorized"),
        "{no_cookie}"
    );

    let no_cookie_command = http_request(
        address,
        &plain_post(
            "/api/v1/commands",
            r#"{"command":"delivery.create","schemaVersion":"winwincode/v1"}"#,
            "https://client.example",
        ),
    )
    .await;
    assert!(
        no_cookie_command.starts_with("HTTP/1.1 401 Unauthorized"),
        "{no_cookie_command}"
    );

    let no_cookie_query = http_request(
        address,
        &plain_post(
            "/api/v1/queries",
            r#"{"query":"settings.get","schemaVersion":"winwincode/v1"}"#,
            "https://client.example",
        ),
    )
    .await;
    assert!(
        no_cookie_query.starts_with("HTTP/1.1 401 Unauthorized"),
        "{no_cookie_query}"
    );

    let no_cookie_password_change = http_request(
        address,
        &plain_post(
            "/api/v1/auth/password",
            r#"{"schemaVersion":"winwincode/v1","currentPassword":"initial-owner-password","newPassword":"rotated-owner-password"}"#,
            "https://client.example",
        ),
    )
    .await;
    assert!(
        no_cookie_password_change.starts_with("HTTP/1.1 401 Unauthorized"),
        "{no_cookie_password_change}"
    );

    let early_login = login_response(address, "owner", OWNER_PASSWORD).await;
    assert!(
        early_login.starts_with("HTTP/1.1 401 Unauthorized"),
        "{early_login}"
    );
    running.shutdown().await.expect("shutdown");
}

#[tokio::test]
async fn initialization_status_is_public_and_reflects_the_durable_marker() {
    let running = start_server(
        config("127.0.0.1:0".parse().expect("address")),
        auth(),
        Arc::new(FakeApi::default()),
    )
    .await
    .expect("start server");
    let address = running.local_address();

    let before = http_request(
        address,
        &origin_get("/api/v1/server/initialization", "https://client.example"),
    )
    .await;
    assert!(before.starts_with("HTTP/1.1 200 OK"), "{before}");
    assert!(before.contains("cache-control: no-store"), "{before}");
    assert_eq!(
        response_json(&before),
        json!({ "schemaVersion": "winwincode/v1", "initialized": false, "authMode": "password" })
    );

    bootstrap_cookie(address).await;

    let after = http_request(
        address,
        &origin_get("/api/v1/server/initialization", "https://client.example"),
    )
    .await;
    assert!(after.starts_with("HTTP/1.1 200 OK"), "{after}");
    assert_eq!(
        response_json(&after),
        json!({ "schemaVersion": "winwincode/v1", "initialized": true, "authMode": "password" })
    );
    running.shutdown().await.expect("shutdown");
}

#[tokio::test]
async fn an_existing_owner_rejects_browser_bootstrap_initialization() {
    let directory = test_directory("winwincode-cli-owner-bootstrap-test");
    let accounts = Arc::new(UserAccountService::open(&directory).expect("account service"));
    accounts
        .initialize_owner(
            OWNER_USERNAME,
            OWNER_PASSWORD,
            &Instant("2027-05-01T08:00:00.000Z".to_owned()),
        )
        .expect("CLI first owner");
    let (_, manager) = open_auth(&directory, "proof-late-bootstrap", vec![fixture_scope(1)]);
    assert!(manager_marker_is_absent(&directory));
    let running = start_server(
        config("127.0.0.1:0".parse().expect("address")),
        manager,
        Arc::new(FakeApi::default()),
    )
    .await
    .expect("start server");
    let address = running.local_address();

    let status = http_request(
        address,
        &origin_get("/api/v1/server/initialization", "https://client.example"),
    )
    .await;
    assert!(status.starts_with("HTTP/1.1 200 OK"), "{status}");
    assert_eq!(response_json(&status)["initialized"], json!(true));

    let bootstrap = initialize_owner(
        address,
        "proof-late-bootstrap",
        "second",
        "second-owner-password",
    )
    .await;
    assert!(
        bootstrap.starts_with("HTTP/1.1 409 Conflict"),
        "{bootstrap}"
    );
    assert!(bootstrap.contains("WRONG_STATE"), "{bootstrap}");

    let (cookie, session) = login_as(address, OWNER_USERNAME, OWNER_PASSWORD).await;
    assert_eq!(session["actor"]["kind"], "user");
    assert!(
        current_session(address, &cookie)
            .await
            .starts_with("HTTP/1.1 200 OK")
    );
    drop(accounts);
    running.shutdown().await.expect("shutdown");
    fs::remove_dir_all(directory).expect("remove CLI-owner directory");
}

fn manager_marker_is_absent(directory: &std::path::Path) -> bool {
    let database_path = directory
        .join("auth-sessions")
        .join("auth-sessions.sqlite3");
    let connection = rusqlite::Connection::open_with_flags(
        database_path,
        rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY,
    )
    .expect("open session store");
    let marker_table: i64 = connection
        .query_row(
            "SELECT EXISTS(SELECT 1 FROM sqlite_master
             WHERE type = 'table' AND name = 'server_initialization')",
            [],
            |row| row.get(0),
        )
        .expect("marker table probe");
    if marker_table == 0 {
        return true;
    }
    let marker_rows: i64 = connection
        .query_row("SELECT COUNT(*) FROM server_initialization", [], |row| {
            row.get(0)
        })
        .expect("marker row count");
    marker_rows == 0
}
