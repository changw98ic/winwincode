// SPDX-License-Identifier: Apache-2.0

use std::collections::BTreeMap;
use std::fmt::Write as _;
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use base64::{Engine as _, engine::general_purpose::STANDARD_NO_PAD as BASE64};
use ed25519_dalek::{Signer as _, SigningKey};
use serde_json::{Value, json};
use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};
use tokio::net::TcpStream;
use winwincode_api::generated::{Actor, OrganizationScope, OrganizationScopeKind, Scope};
use winwincode_client_port::device_auth::signing_bytes;
use winwincode_domain::{OrganizationId, UserActor, UserActorKind, UserId};
use winwincode_server::{
    ApiError, AuthError, AuthSessionBootstrap, AuthSessionConfig, AuthenticatedPrincipal,
    ClientExchangeApplication, ClientExchangeConfig, ControlPlaneApiPort, EventSubscription,
    RequestAuthenticator, RunningServer, ServerConfig, SqliteAuthSessionManager,
    TransportCredentials, UserAccountService, start_server_with_remote_worker,
};

const ORIGIN: &str = "http://localhost:8080";

fn scope() -> Scope {
    Scope::OrganizationScope(OrganizationScope {
        kind: OrganizationScopeKind::Organization,
        organization_id: OrganizationId("org_00000000000000000000000001".to_owned()),
    })
}

struct Authenticator {
    sessions: Arc<SqliteAuthSessionManager>,
}
impl RequestAuthenticator for Authenticator {
    fn authenticate(
        &self,
        credentials: &TransportCredentials,
    ) -> Result<AuthenticatedPrincipal, AuthError> {
        if credentials.session_cookie() == Some("non-owner") {
            return AuthenticatedPrincipal::new(
                Actor::UserActor(UserActor {
                    kind: UserActorKind::User,
                    id: UserId("usr_00000000000000000000000009".to_owned()),
                }),
                vec![scope()],
            );
        }
        self.sessions.authenticate(credentials)
    }
}

struct Api;
impl ControlPlaneApiPort for Api {
    fn command(&self, _: &AuthenticatedPrincipal, _: Value) -> Result<Value, ApiError> {
        Err(ApiError::new(404, "NOT_FOUND", "unavailable"))
    }
    fn query(&self, _: &AuthenticatedPrincipal, _: Value) -> Result<Value, ApiError> {
        Err(ApiError::new(404, "NOT_FOUND", "unavailable"))
    }
    fn subscribe(
        &self,
        _: &AuthenticatedPrincipal,
        _: Value,
    ) -> Result<EventSubscription, ApiError> {
        Err(ApiError::new(404, "NOT_FOUND", "unavailable"))
    }
    fn event_control(&self, _: &AuthenticatedPrincipal, _: Value) -> Result<Vec<Value>, ApiError> {
        Err(ApiError::new(404, "NOT_FOUND", "unavailable"))
    }
    fn shutdown(&self) -> Result<(), ApiError> {
        Ok(())
    }
}

async fn request(
    server: &RunningServer,
    path: &str,
    body: &[u8],
    headers: &[(&str, String)],
) -> (u16, BTreeMap<String, String>, Value) {
    parse_response(&request_raw(server.local_address(), path, body, headers).await)
}

async fn request_raw(
    address: SocketAddr,
    path: &str,
    body: &[u8],
    headers: &[(&str, String)],
) -> Vec<u8> {
    let mut stream = TcpStream::connect(address).await.unwrap();
    let mut head = format!(
        "POST {path} HTTP/1.1\r\nHost: localhost\r\nOrigin: {ORIGIN}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n",
        body.len()
    );
    for (name, value) in headers {
        writeln!(head, "{name}: {value}\r").unwrap();
    }
    head.push_str("\r\n");
    stream.write_all(head.as_bytes()).await.unwrap();
    stream.write_all(body).await.unwrap();
    let mut response = Vec::new();
    let _ = stream.read_to_end(&mut response).await;
    response
}

fn parse_response(response: &[u8]) -> (u16, BTreeMap<String, String>, Value) {
    let response = std::str::from_utf8(response).unwrap();
    let (head, body) = response.split_once("\r\n\r\n").unwrap();
    let mut lines = head.split("\r\n");
    let status = lines
        .next()
        .unwrap()
        .split_whitespace()
        .nth(1)
        .unwrap()
        .parse()
        .unwrap();
    let headers = lines
        .map(|line| {
            let (name, value) = line.split_once(':').unwrap();
            (name.to_lowercase(), value.trim().to_owned())
        })
        .collect();
    (status, headers, serde_json::from_str(body).unwrap())
}

async fn bind(server: &RunningServer, key: &SigningKey, body: &Value) -> (u16, Value) {
    let (status, _, value) = parse_response(&bind_raw(server.local_address(), key, body).await);
    (status, value)
}

async fn bind_raw(address: SocketAddr, key: &SigningKey, body: &Value) -> Vec<u8> {
    let public_key = BASE64.encode(key.verifying_key().as_bytes());
    let challenge_body =
        serde_json::to_vec(&json!({"schemaVersion":"winwincode/v1","publicKey":public_key}))
            .unwrap();
    let (status, _, challenge) = parse_response(
        &request_raw(
            address,
            "/internal/v1/device/challenge",
            &challenge_body,
            &[],
        )
        .await,
    );
    assert_eq!(status, 200);
    let body = serde_json::to_vec(body).unwrap();
    let challenge_id = challenge["challengeId"].as_str().unwrap();
    let signature = key.sign(&signing_bytes(
        challenge_id,
        challenge["challenge"].as_str().unwrap(),
        "POST",
        "/internal/v1/device/bind",
        "",
        &body,
    ));
    request_raw(
        address,
        "/internal/v1/device/bind",
        &body,
        &[
            ("x-winwincode-device-public-key", public_key),
            ("x-winwincode-device-challenge-id", challenge_id.to_owned()),
            (
                "x-winwincode-device-signature",
                BASE64.encode(signature.to_bytes()),
            ),
        ],
    )
    .await
}

fn assert_error(status: u16, value: &Value, expected_status: u16, code: &str) {
    assert_eq!(status, expected_status);
    assert_eq!(*value, json!({"schemaVersion":"winwincode/v1","code":code}));
}

#[tokio::test]
#[allow(clippy::too_many_lines)]
async fn device_routes_return_exact_auth_errors_and_revocation_rejects_signed_recovery() {
    let directory = std::env::temp_dir().join(format!(
        "wwc-device-http-{}-{}",
        std::process::id(),
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    let accounts = Arc::new(UserAccountService::open(&directory).unwrap());
    let sessions = Arc::new(
        SqliteAuthSessionManager::open(
            directory.join("auth-sessions"),
            vec![AuthSessionBootstrap::new("bootstrap-proof").unwrap()],
            vec![scope()],
            AuthSessionConfig::default(),
            accounts,
            None,
        )
        .unwrap(),
    );
    let config = ServerConfig::new(
        "127.0.0.1:0".parse().unwrap(),
        "http://127.0.0.1:8443",
        [ORIGIN.to_owned()],
        directory.clone(),
        Duration::from_secs(2),
    )
    .unwrap();
    let server = start_server_with_remote_worker(
        config,
        Arc::clone(&sessions),
        Arc::new(Authenticator { sessions }),
        Arc::new(Api),
        None,
        Some(Arc::new(
            ClientExchangeApplication::open(&directory, &ClientExchangeConfig::default()).unwrap(),
        )),
    )
    .await
    .unwrap();
    let valid = serde_json::to_vec(&json!({"schemaVersion":"winwincode/v1"})).unwrap();
    for path in [
        "/api/v1/devices/pairing",
        "/api/v1/devices/cnd_00000000000000000000000001/revoke",
    ] {
        let (status, _, value) = request(&server, path, &valid, &[]).await;
        assert_error(status, &value, 401, "DEVICE_AUTH_REQUIRED");
    }
    let init=serde_json::to_vec(&json!({"schemaVersion":"winwincode/v1","username":"Owner","password":"initial-owner-password"})).unwrap();
    let (status, headers, _) = request(
        &server,
        "/api/v1/auth/session",
        &init,
        &[("Authorization", "Bearer bootstrap-proof".to_owned())],
    )
    .await;
    assert_eq!(status, 201);
    let cookie = headers["set-cookie"].split(';').next().unwrap().to_owned();
    let cookie_name = cookie.split('=').next().unwrap();
    let owner = [("Cookie", cookie.clone())];
    let other = [("Cookie", format!("{cookie_name}=non-owner"))];
    for path in [
        "/api/v1/devices/pairing",
        "/api/v1/devices/cnd_00000000000000000000000001/revoke",
    ] {
        let (status, _, value) = request(&server, path, &valid, &other).await;
        assert_error(status, &value, 403, "PERMISSION_DENIED");
        let (status, _, value) = request(&server, path, b"", &owner).await;
        assert_error(status, &value, 400, "INVALID_REQUEST");
        let (status, _, value) =
            request(&server, &format!("{path}?unexpected=1"), &valid, &owner).await;
        assert_error(status, &value, 400, "INVALID_REQUEST");
    }
    for path in ["/internal/v1/device/challenge", "/internal/v1/device/bind"] {
        let (status, _, value) = request(&server, path, b"", &[]).await;
        assert_error(status, &value, 400, "INVALID_REQUEST");
        let (status, _, value) = request(&server, path, b"{}", &[]).await;
        assert_error(status, &value, 400, "INVALID_REQUEST");
        let (status, _, value) =
            request(&server, &format!("{path}?unexpected=1"), &valid, &[]).await;
        assert_error(status, &value, 400, "INVALID_REQUEST");
    }
    let (status, _, pairing) = request(&server, "/api/v1/devices/pairing", &valid, &owner).await;
    assert_eq!(status, 201);
    let key = SigningKey::from_bytes(&[9; 32]);
    let binding = json!({"schemaVersion":"winwincode/v1","requestId":"dbr_00000000000000000000000001","pairingToken":pairing["pairingToken"],"publicKey":BASE64.encode(key.verifying_key().as_bytes()),"intent":{"displayName":"Device","platform":"aarch64-apple-darwin","architecture":"aarch64","clientVersion":"0.2.0"}});
    let (status, _, value) = request(
        &server,
        "/internal/v1/device/bind",
        &serde_json::to_vec(&binding).unwrap(),
        &[],
    )
    .await;
    assert_error(status, &value, 401, "DEVICE_AUTH_REQUIRED");
    let (status, first) = bind(&server, &key, &binding).await;
    assert_eq!(status, 200);
    let (status, recovered) = bind(&server, &key, &binding).await;
    assert_eq!(status, 200);
    assert_eq!(recovered, first);
    let (status, _, revoked) = request(
        &server,
        &format!(
            "/api/v1/devices/{}/revoke",
            first["clientNodeId"].as_str().unwrap()
        ),
        &valid,
        &owner,
    )
    .await;
    assert_eq!(status, 200);
    assert_eq!(revoked["revoked"], true);
    let (status, value) = bind(&server, &key, &binding).await;
    assert_error(status, &value, 403, "DEVICE_REVOKED");
    for rejected in ["wrong", "expired", "short", "long"] {
        let (status, _, pairing) =
            request(&server, "/api/v1/devices/pairing", &valid, &owner).await;
        assert_eq!(status, 201);
        let token = if rejected == "expired" {
            let storage = winwincode_storage::SqliteStorage::open(&directory).unwrap();
            rusqlite::Connection::open(storage.database_path())
                .unwrap()
                .execute(
                    "UPDATE device_pairing_tokens SET expires_at='2000-01-01T00:00:00.000Z' WHERE consumed_request_id IS NULL",
                    [],
                )
                .unwrap();
            pairing["pairingToken"].as_str().unwrap().to_owned()
        } else {
            match rejected {
                "short" => "s".repeat(7),
                "long" => "l".repeat(201),
                _ => "wrong-pairing-token".to_owned(),
            }
        };
        let mut rejected_request = binding.clone();
        rejected_request["requestId"] = json!(format!("rejected-pairing-{rejected}"));
        rejected_request["pairingToken"] = json!(token);
        let device_directory = directory.join(format!("device-{rejected}"));
        let config = winwincode_device_client::DeviceBindConfig {
            data_directory: device_directory.clone(),
            server_url: Some(format!("http://{}", server.local_address())),
            pairing_token: Some(token),
            ca_file: None,
            seed: winwincode_device_client::DeviceIdentitySeed {
                display_name: "Rejected pairing fixture".to_owned(),
                platform: "aarch64-apple-darwin".to_owned(),
                architecture: "aarch64".to_owned(),
                client_version: "0.2.0".to_owned(),
            },
        };
        let first = config.clone();
        assert!(
            tokio::task::spawn_blocking(move || winwincode_device_client::bind_device(&first))
                .await
                .unwrap()
                .is_err()
        );
        let store = winwincode_device_client::DeviceStore::open(&device_directory).unwrap();
        let pending = winwincode_device_client::load_remote_binding(&store)
            .unwrap()
            .unwrap();
        assert!(!pending.is_bound());
        assert!(pending.failure_reason.is_none());
        rusqlite::Connection::open(store.database_path())
            .unwrap()
            .execute(
                "UPDATE remote_device_binding SET failure_reason='Device 身份认证失败；服务已停止。' WHERE singleton=1",
                [],
            )
            .unwrap();
        let (status, _, pairing) =
            request(&server, "/api/v1/devices/pairing", &valid, &owner).await;
        assert_eq!(status, 201);
        let resumed = winwincode_device_client::DeviceBindConfig {
            server_url: None,
            pairing_token: Some(pairing["pairingToken"].as_str().unwrap().to_owned()),
            ..config
        };
        let receipt =
            tokio::task::spawn_blocking(move || winwincode_device_client::bind_device(&resumed))
                .await
                .unwrap()
                .unwrap_or_else(|error| {
                    panic!("same-directory replacement after {rejected} token failed: {error}")
                });
        assert_eq!(receipt.request_id.0, pending.request_id);
        let stored = winwincode_device_client::load_remote_binding(&store)
            .unwrap()
            .unwrap();
        assert!(stored.is_bound());
        let storage = winwincode_storage::SqliteStorage::open(&directory).unwrap();
        let database = rusqlite::Connection::open(storage.database_path()).unwrap();
        let count: i64 = database
            .query_row(
                "SELECT count(*) FROM device_identity_bindings WHERE request_id=?1",
                [&pending.request_id],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(count, 1);
        let (status, value) = bind(&server, &key, &rejected_request).await;
        assert_error(status, &value, 401, "DEVICE_PAIRING_REJECTED");
    }
    server.shutdown().await.unwrap();
    std::fs::remove_dir_all(directory).unwrap();
}

#[cfg(feature = "test-support")]
#[tokio::test]
#[allow(clippy::too_many_lines)]
async fn committed_binding_survives_real_response_transport_failure() {
    struct Process(std::process::Child);
    impl Drop for Process {
        fn drop(&mut self) {
            let _ = self.0.kill();
            let _ = self.0.wait();
        }
    }
    let directory = std::env::temp_dir().join(format!(
        "wwc-device-response-loss-{}-{}",
        std::process::id(),
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    let repository = directory.join("repository");
    let data = directory.join("server");
    std::fs::create_dir_all(&repository).unwrap();
    assert!(
        std::process::Command::new("git")
            .args(["init", "--quiet"])
            .current_dir(&repository)
            .status()
            .unwrap()
            .success()
    );
    assert!(
        std::process::Command::new("git")
            .args([
                "-c",
                "user.name=Test",
                "-c",
                "user.email=test@example.test",
                "commit",
                "--quiet",
                "--allow-empty",
                "-m",
                "fixture"
            ])
            .current_dir(&repository)
            .status()
            .unwrap()
            .success()
    );
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let address = listener.local_addr().unwrap();
    drop(listener);
    let log = directory.join("server.stderr");
    let mut process = Process(
        std::process::Command::new(
            std::env::var_os("CARGO_BIN_EXE_winwincode-server").expect("Cargo binary path"),
        )
        .env("WWC_SERVER_BIND", address.to_string())
        .env("WWC_SERVER_PUBLIC_URL", format!("http://{address}"))
        .env("WWC_SERVER_AUTH_MODE", "local-open")
        .env("WWC_SERVER_ALLOWED_ORIGINS", ORIGIN)
        .env("WWC_SERVER_DATA_DIRECTORY", &data)
        .env("WWC_SERVER_REPOSITORY_ROOT", &repository)
        .env("WWC_TEST_DROP_FIRST_DEVICE_BIND_RESPONSE", "1")
        .env_remove("WWC_SERVER_TLS_CERTIFICATE")
        .env_remove("WWC_SERVER_TLS_PRIVATE_KEY")
        .env_remove("WWC_SERVER_TLS_PROXY")
        .env_remove("WWC_SERVER_PREVIEW_PUBLIC_URL")
        .stdout(std::process::Stdio::null())
        .stderr(std::fs::File::create(&log).unwrap())
        .spawn()
        .unwrap(),
    );
    let deadline = tokio::time::Instant::now() + Duration::from_secs(20);
    loop {
        if TcpStream::connect(address).await.is_ok() {
            break;
        }
        assert!(
            process.0.try_wait().unwrap().is_none(),
            "{}",
            std::fs::read_to_string(&log).unwrap()
        );
        assert!(
            tokio::time::Instant::now() < deadline,
            "Server did not start"
        );
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    let valid = serde_json::to_vec(&json!({"schemaVersion":"winwincode/v1"})).unwrap();
    let (status, _, pairing) =
        parse_response(&request_raw(address, "/api/v1/devices/pairing", &valid, &[]).await);
    assert_eq!(status, 201);
    let key = SigningKey::from_bytes(&[10; 32]);
    let binding = json!({"schemaVersion":"winwincode/v1","requestId":"dbr_00000000000000000000000002","pairingToken":pairing["pairingToken"],"publicKey":BASE64.encode(key.verifying_key().as_bytes()),"intent":{"displayName":"Device","platform":"aarch64-apple-darwin","architecture":"aarch64","clientVersion":"0.2.0"}});
    let first = bind_raw(address, &key, &binding).await;
    let incomplete = std::str::from_utf8(&first)
        .ok()
        .and_then(|raw| raw.split_once("\r\n\r\n"))
        .is_none_or(|(_, body)| serde_json::from_str::<Value>(body).is_err());
    assert!(incomplete, "the first response must fail in transport");
    let storage = winwincode_storage::SqliteStorage::open(&data).unwrap();
    let database = rusqlite::Connection::open(storage.database_path()).unwrap();
    let receipt: String = database
        .query_row(
            "SELECT response_json FROM device_identity_bindings WHERE request_id=?1",
            ["dbr_00000000000000000000000002"],
            |row| row.get(0),
        )
        .unwrap();
    let stored: Value = serde_json::from_str(&receipt).unwrap();
    assert_eq!(stored.as_object().unwrap().len(), 6);
    let (status, _, recovered) = parse_response(&bind_raw(address, &key, &binding).await);
    assert_eq!(status, 200);
    assert_eq!(recovered, stored);
    let count: i64 = database
        .query_row("SELECT count(*) FROM device_identity_bindings", [], |row| {
            row.get(0)
        })
        .unwrap();
    assert_eq!(count, 1);
    assert!(
        std::fs::read_to_string(&log)
            .unwrap()
            .contains("test-support dropped committed device binding response")
    );
    process.0.kill().unwrap();
    process.0.wait().unwrap();
    drop(database);
    drop(storage);
    std::fs::remove_dir_all(directory).unwrap();
}
