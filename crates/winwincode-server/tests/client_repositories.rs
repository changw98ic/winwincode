// SPDX-License-Identifier: Apache-2.0

//! The repository directory route over real HTTP (REPO-100.4): the
//! `GET /api/v1/repositories?clientId=…` projection of the durable repository
//! bindings (plan 13.4, REPO-100.3 facade shape), covering the sign-in
//! requirement, the query identity validation, the dual Client+Repository
//! grant visibility, per-user directory sets, and the revalidate evidence:
//! an availability change applied through the repository service is consumed
//! by the next directory read.

use std::collections::BTreeSet;
use std::path::Path;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::OnceLock;
use std::sync::atomic::AtomicU64;
use std::sync::atomic::Ordering;
use std::time::Duration;

use serde_json::Value;
use tokio::io::AsyncReadExt;
use tokio::io::AsyncWriteExt;
use tokio::net::TcpStream;
use tokio::sync::mpsc;
use winwincode_api::generated::{OrganizationScope, OrganizationScopeKind, Scope};
use winwincode_control_plane::{
    ClientRegistryService, RepositoryAccessGrantService, RepositoryBindingService,
};
use winwincode_domain::Instant;
use winwincode_server::{
    ApiError, AuthSessionBootstrap, AuthSessionConfig, AuthenticatedPrincipal,
    ClientExchangeApplication, ClientExchangeConfig, ClientExchangePort, ControlPlaneApiPort,
    EventSubscription, RequestAuthenticator, ServerConfig, ServerTls, SqliteAuthSessionManager,
    UserAccountService, start_server_with_remote_worker,
};
use winwincode_storage::{
    AccessGrantIssuance, ClientNodeRegistration, ClientPresenceState, GrantPermissions,
    GrantSource, GrantTrustMode, RepositoryAccessGrantIssuance, RepositoryBindingProjection,
    RepositoryGrantPermissions, RepositoryScanOutcome, SqliteStorage,
};

const BOOTSTRAP_PROOF: &str = "client-repositories-test-bootstrap";
const ORIGIN: &str = "https://repo.example";
const OWNER_PASSWORD: &str = "initial-owner-password";
const PUBLIC_CLIENT_ID: &str = "927351842";
const NODE: &str = "cnd_A1A1A1A1A1A1A1A1A1A1A1A1A1";
const T0: &str = "2026-09-04T12:00:00.000Z";
const SCHEMA_VERSION: &str = "winwincode/v1";

static NEXT_DIRECTORY: AtomicU64 = AtomicU64::new(1);
static NEXT_GRANT: AtomicU64 = AtomicU64::new(1);
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

fn fresh_grant_id(prefix: &str) -> String {
    format!(
        "{prefix}_{:026}",
        NEXT_GRANT.fetch_add(1, Ordering::Relaxed)
    )
}

fn alpha_binding_id() -> String {
    format!("rbd_{:026}", 1)
}

fn beta_binding_id() -> String {
    format!("rbd_{:026}", 2)
}

fn instant(value: &str) -> Instant {
    Instant(value.to_owned())
}

#[derive(Default)]
struct NoopApi;

impl ControlPlaneApiPort for NoopApi {
    fn command(&self, _: &AuthenticatedPrincipal, _: Value) -> Result<Value, ApiError> {
        Err(ApiError::new(
            501,
            "NOT_IMPLEMENTED",
            "unused in repository directory tests",
        ))
    }

    fn query(&self, _: &AuthenticatedPrincipal, _: Value) -> Result<Value, ApiError> {
        Err(ApiError::new(
            501,
            "NOT_IMPLEMENTED",
            "unused in repository directory tests",
        ))
    }

    fn subscribe(
        &self,
        _: &AuthenticatedPrincipal,
        first_frame: Value,
    ) -> Result<EventSubscription, ApiError> {
        let (_, receiver) = mpsc::channel(1);
        Ok(EventSubscription {
            initial_frames: vec![first_frame],
            events: receiver,
        })
    }

    fn event_control(
        &self,
        _: &AuthenticatedPrincipal,
        frame: Value,
    ) -> Result<Vec<Value>, ApiError> {
        Ok(vec![frame])
    }

    fn shutdown(&self) -> Result<(), ApiError> {
        Ok(())
    }
}

fn server_config(data_directory: &Path) -> ServerConfig {
    ServerConfig::new(
        "127.0.0.1:0".parse().expect("loopback address"),
        "http://control.example",
        ServerTls::Disabled,
        BTreeSet::from([ORIGIN.to_owned()]),
        data_directory.to_path_buf(),
        Duration::from_secs(2),
    )
    .expect("valid config")
}

fn open_auth(directory: &Path) -> Arc<SqliteAuthSessionManager> {
    let scopes = vec![Scope::OrganizationScope(OrganizationScope {
        kind: OrganizationScopeKind::Organization,
        organization_id: winwincode_domain::OrganizationId(
            "org_00000000000000000000000001".to_owned(),
        ),
    })];
    let accounts = Arc::new(UserAccountService::open(directory).expect("account service"));
    Arc::new(
        SqliteAuthSessionManager::open(
            directory.join("auth-sessions"),
            vec![AuthSessionBootstrap::new(BOOTSTRAP_PROOF).expect("proof")],
            scopes,
            AuthSessionConfig::default(),
            accounts,
            None,
        )
        .expect("auth session manager"),
    )
}

async fn start_server(
    data_directory: &Path,
    auth_directory: &Path,
) -> winwincode_server::RunningServer {
    let exchange: Arc<dyn ClientExchangePort> = Arc::new(
        ClientExchangeApplication::open(data_directory, &ClientExchangeConfig::default())
            .expect("valid client exchange application"),
    );
    let sessions = open_auth(auth_directory);
    let authenticator: Arc<dyn RequestAuthenticator> = sessions.clone();
    start_server_with_remote_worker(
        server_config(data_directory),
        sessions,
        authenticator,
        Arc::new(NoopApi),
        None,
        None,
        Some(exchange),
    )
    .await
    .expect("start server with client surface")
}

async fn http_request(address: std::net::SocketAddr, request: &str) -> String {
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

fn repositories_get(path: &str, cookie: &str) -> String {
    format!(
        "GET {path} HTTP/1.1\r\nHost: control.example\r\nOrigin: {ORIGIN}\r\nCookie: wwc_session={cookie}\r\nConnection: close\r\n\r\n"
    )
}

/// Same GET with no credential at all.
fn anonymous_get(path: &str) -> String {
    format!(
        "GET {path} HTTP/1.1\r\nHost: control.example\r\nOrigin: {ORIGIN}\r\nConnection: close\r\n\r\n"
    )
}

fn plain_post(path: &str, body: &str) -> String {
    format!(
        "POST {path} HTTP/1.1\r\nHost: control.example\r\nOrigin: {ORIGIN}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    )
}

fn bearer_post(path: &str, body: &str, proof: &str) -> String {
    format!(
        "POST {path} HTTP/1.1\r\nHost: control.example\r\nOrigin: {ORIGIN}\r\nAuthorization: Bearer {proof}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    )
}

fn cookie_post(path: &str, body: &str, cookie: &str) -> String {
    format!(
        "POST {path} HTTP/1.1\r\nHost: control.example\r\nOrigin: {ORIGIN}\r\nCookie: wwc_session={cookie}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    )
}

fn status_of(response: &str) -> String {
    response
        .lines()
        .next()
        .expect("status line")
        .split_whitespace()
        .nth(1)
        .expect("status code")
        .to_owned()
}

fn response_body(response: &str) -> Value {
    let (_, body) = response
        .split_once("\r\n\r\n")
        .expect("HTTP response has a body boundary");
    serde_json::from_str(body).expect("HTTP response contains JSON")
}

fn wire_code(response: &str) -> String {
    response_body(response)["error"]["code"]
        .as_str()
        .expect("error code")
        .to_owned()
}

fn session_cookie_from_response(response: &str) -> String {
    let set_cookie = response
        .lines()
        .find_map(|line| line.strip_prefix("set-cookie: "))
        .expect("session Set-Cookie header");
    let pair = set_cookie.split(';').next().expect("cookie pair");
    pair.strip_prefix("wwc_session=")
        .expect("session cookie name")
        .to_owned()
}

fn login_body(username: &str, password: &str) -> String {
    serde_json::json!({
        "schemaVersion": SCHEMA_VERSION,
        "username": username,
        "password": password,
    })
    .to_string()
}

/// Initializes the Owner with a password, then signs in; returns
/// (cookie, userId).
async fn initialize_and_login_owner(address: std::net::SocketAddr) -> (String, String) {
    let response = http_request(
        address,
        &bearer_post(
            "/api/v1/auth/session",
            &login_body("owner", OWNER_PASSWORD),
            BOOTSTRAP_PROOF,
        ),
    )
    .await;
    assert!(
        response.starts_with("HTTP/1.1 201"),
        "bootstrap must succeed: {response}"
    );
    login(address, "owner", OWNER_PASSWORD).await
}

async fn login(address: std::net::SocketAddr, username: &str, password: &str) -> (String, String) {
    let response = http_request(
        address,
        &plain_post("/api/v1/auth/session", &login_body(username, password)),
    )
    .await;
    assert!(response.starts_with("HTTP/1.1 201"), "{response}");
    let user_id = response_body(&response)["actor"]["id"]
        .as_str()
        .expect("actor id")
        .to_owned();
    (session_cookie_from_response(&response), user_id)
}

/// Creates one member account and signs in; returns (cookie, userId).
async fn create_and_login_member(
    address: std::net::SocketAddr,
    owner_cookie: &str,
    username: &str,
) -> (String, String) {
    let create = serde_json::json!({
        "schemaVersion": SCHEMA_VERSION,
        "username": username,
        "role": "member",
    })
    .to_string();
    let response = http_request(
        address,
        &cookie_post("/api/v1/users", &create, owner_cookie),
    )
    .await;
    assert!(response.starts_with("HTTP/1.1 201"), "{response}");
    let temporary = response_body(&response)["temporaryPassword"]
        .as_str()
        .expect("temporary password")
        .to_owned();
    login(address, username, &temporary).await
}

// ---- durable staging (the Device-Client side of the projection) -----------

/// Seeds one registered, `online` Client node under a fixed public id.
fn stage_device(data_directory: &Path) {
    let mut storage = SqliteStorage::open(data_directory).expect("storage");
    let mut registry = ClientRegistryService::new(&mut storage);
    let registration = ClientNodeRegistration::try_new(
        NODE,
        PUBLIC_CLIENT_ID,
        "Cheng's MacBook",
        "aarch64-apple-darwin",
        "aarch64",
        "0.1.0-alpha.1",
        None,
        None,
        4,
    )
    .expect("registration");
    registry
        .register(&registration, 0, &instant(T0))
        .expect("register");
    registry
        .update_presence(NODE, ClientPresenceState::Online, 1)
        .expect("presence online");
}

/// Reports one binding the way the Device Client does at launch (available,
/// clean); returns the durable revision.
fn upsert_binding(
    data_directory: &Path,
    binding_id: &str,
    display_name: &str,
    default_branch: &str,
    head_commit: &str,
    fingerprint: &str,
) -> u64 {
    let mut storage = SqliteStorage::open(data_directory).expect("storage");
    let mut bindings = RepositoryBindingService::new(&mut storage);
    let projection = RepositoryBindingProjection::try_new(
        binding_id,
        NODE,
        display_name,
        Some(default_branch.to_owned()),
        Some(head_commit.to_owned()),
        winwincode_storage::RepositoryDirtyState::Clean,
        winwincode_storage::RepositoryAvailability::Available,
        fingerprint,
    )
    .expect("projection");
    bindings
        .upsert(&projection, Some(&instant(T0)), 0, &instant(T0))
        .expect("upsert")
        .record
        .revision
}

/// Creates one active `use` Client access grant for `user_id`.
fn stage_client_grant(data_directory: &Path, node: &str, user_id: &str) {
    let mut storage = SqliteStorage::open(data_directory).expect("storage");
    let issuance = AccessGrantIssuance::try_new(
        fresh_grant_id("cag"),
        node,
        user_id,
        user_id,
        GrantTrustMode::Trusted,
        None,
    )
    .expect("client grant issuance");
    storage
        .client_connect_ledger()
        .expect("connect ledger")
        .create_grant(
            &issuance,
            GrantSource::Administrator,
            GrantPermissions::USE,
            &instant(T0),
        )
        .expect("client grant");
}

/// Creates one active repository access grant for `user_id`.
fn stage_repository_grant(data_directory: &Path, binding_id: &str, user_id: &str) {
    let mut storage = SqliteStorage::open(data_directory).expect("storage");
    let mut grants = RepositoryAccessGrantService::new(&mut storage);
    let issuance =
        RepositoryAccessGrantIssuance::try_new(fresh_grant_id("rag"), binding_id, user_id, user_id)
            .expect("repository grant issuance");
    grants
        .create_grant(&issuance, RepositoryGrantPermissions::Use, &instant(T0))
        .expect("repository grant");
}

/// Applies one rescan outcome through the repository service (the server-side
/// consumption of a device revalidation); returns the new revision. A
/// non-`available` outcome reports a dirty work tree, which the frozen
/// consistency rule accepts for every non-`available` state.
fn apply_rescan(
    data_directory: &Path,
    binding_id: &str,
    availability: winwincode_storage::RepositoryAvailability,
    expected_revision: u64,
) -> u64 {
    let dirty_state = if availability == winwincode_storage::RepositoryAvailability::Available {
        winwincode_storage::RepositoryDirtyState::Clean
    } else {
        winwincode_storage::RepositoryDirtyState::Dirty
    };
    let mut storage = SqliteStorage::open(data_directory).expect("storage");
    let mut bindings = RepositoryBindingService::new(&mut storage);
    let outcome = RepositoryScanOutcome::try_new(availability, dirty_state).expect("scan outcome");
    bindings
        .update_availability(binding_id, &outcome, &instant(T0), expected_revision)
        .expect("availability update")
        .revision
}

// ---- tests ----------------------------------------------------------------

#[tokio::test]
async fn repository_directory_requires_a_signed_in_session() {
    let data_directory = test_directory("repo-dir-auth");
    let auth_directory = test_directory("repo-dir-auth-sessions");
    let running = start_server(&data_directory, &auth_directory).await;
    let address = running.local_address();

    let response = http_request(
        address,
        &repositories_get("/api/v1/repositories?clientId=927351842", "not-a-session"),
    )
    .await;
    assert_eq!(status_of(&response), "401", "{response}");
    assert_eq!(wire_code(&response), "AUTHENTICATION_REQUIRED");

    // No credential at all is the same rejection.
    let response = http_request(
        address,
        &anonymous_get("/api/v1/repositories?clientId=927351842"),
    )
    .await;
    assert_eq!(status_of(&response), "401", "{response}");
    assert_eq!(wire_code(&response), "AUTHENTICATION_REQUIRED");

    running.shutdown().await.expect("shutdown");
    let _ = std::fs::remove_dir_all(&data_directory);
    let _ = std::fs::remove_dir_all(&auth_directory);
}

#[tokio::test]
async fn repository_directory_rejects_malformed_and_unknown_client_identities() {
    let data_directory = test_directory("repo-dir-identity");
    let auth_directory = test_directory("repo-dir-identity-sessions");
    let running = start_server(&data_directory, &auth_directory).await;
    let address = running.local_address();
    let (cookie, _user_id) = initialize_and_login_owner(address).await;

    for (query, description) in [
        ("/api/v1/repositories", "missing query"),
        ("/api/v1/repositories?", "empty query"),
        ("/api/v1/repositories?clientId=", "empty id"),
        ("/api/v1/repositories?clientId=12ab", "non-digit id"),
        ("/api/v1/repositories?clientId=1&clientId=2", "repeated id"),
        ("/api/v1/repositories?foo=1", "wrong parameter"),
        (
            "/api/v1/repositories?clientId=927351842&extra=1",
            "extra parameter",
        ),
        ("/api/v1/repositories?clientid=927351842", "wrong name"),
    ] {
        let response = http_request(address, &repositories_get(query, &cookie)).await;
        assert_eq!(status_of(&response), "400", "{description}: {response}");
        assert_eq!(wire_code(&response), "INVALID_REQUEST", "{description}");
    }

    let response = http_request(
        address,
        &repositories_get("/api/v1/repositories?clientId=000000000", &cookie),
    )
    .await;
    assert_eq!(status_of(&response), "404", "{response}");
    assert_eq!(wire_code(&response), "CLIENT_NOT_FOUND");

    running.shutdown().await.expect("shutdown");
    let _ = std::fs::remove_dir_all(&data_directory);
    let _ = std::fs::remove_dir_all(&auth_directory);
}

/// The dual-authorization matrix (plan 13.4) plus the field-by-field facade
/// shape and per-user directory sets:
///
/// - the Owner holds the Client grant and the repository grant on `alpha`, so
///   `alpha` is visible and `beta` (repository grant missing) is not;
/// - the Member holds the Client grant and the repository grant on `beta`, so
///   their directory is different from the Owner's;
/// - the Spectator holds a repository grant on `alpha` but no Client grant,
///   so nothing is visible.
#[tokio::test]
#[allow(clippy::too_many_lines)]
async fn repository_directory_follows_the_dual_authorization_and_facade_shape() {
    let data_directory = test_directory("repo-dir-acl");
    let auth_directory = test_directory("repo-dir-acl-sessions");
    let running = start_server(&data_directory, &auth_directory).await;
    let address = running.local_address();
    let (owner_cookie, owner_id) = initialize_and_login_owner(address).await;
    let (member_cookie, member_id) =
        create_and_login_member(address, &owner_cookie, "member").await;
    let (spectator_cookie, spectator_id) =
        create_and_login_member(address, &owner_cookie, "spectator").await;

    stage_device(&data_directory);
    upsert_binding(
        &data_directory,
        &alpha_binding_id(),
        "Alpha Repo",
        "main",
        &"a".repeat(40),
        "fingerprint-alpha",
    );
    upsert_binding(
        &data_directory,
        &beta_binding_id(),
        "Beta Repo",
        "trunk",
        &"b".repeat(64),
        "fingerprint-beta",
    );
    stage_client_grant(&data_directory, NODE, &owner_id);
    stage_client_grant(&data_directory, NODE, &member_id);
    stage_repository_grant(&data_directory, &alpha_binding_id(), &owner_id);
    stage_repository_grant(&data_directory, &beta_binding_id(), &member_id);
    stage_repository_grant(&data_directory, &alpha_binding_id(), &spectator_id);

    // The Owner sees exactly alpha, field by field in the facade shape.
    let response = http_request(
        address,
        &repositories_get("/api/v1/repositories?clientId=927351842", &owner_cookie),
    )
    .await;
    assert_eq!(status_of(&response), "200", "{response}");
    let value = response_body(&response);
    assert_eq!(value["schemaVersion"], serde_json::json!(SCHEMA_VERSION));
    let mut top_level = value
        .as_object()
        .expect("body object")
        .keys()
        .map(String::as_str)
        .collect::<Vec<_>>();
    top_level.sort_unstable();
    assert_eq!(top_level, vec!["repositories", "schemaVersion"]);
    let repositories = value["repositories"]
        .as_array()
        .expect("repositories array");
    assert_eq!(repositories.len(), 1, "owner sees only alpha: {value}");
    let card = &repositories[0];
    let mut field_names = card
        .as_object()
        .expect("card object")
        .keys()
        .map(String::as_str)
        .collect::<Vec<_>>();
    field_names.sort_unstable();
    assert_eq!(
        field_names,
        vec![
            "availability",
            "defaultBranch",
            "dirtyState",
            "displayName",
            "headCommit",
            "repositoryBindingId",
        ]
    );
    assert_eq!(card["repositoryBindingId"], alpha_binding_id().as_str());
    assert_eq!(card["displayName"], "Alpha Repo");
    assert_eq!(card["defaultBranch"], "main");
    assert_eq!(card["headCommit"], "a".repeat(40));
    assert_eq!(card["dirtyState"], "clean");
    assert_eq!(card["availability"], "available");

    // The Member's directory is a different set: beta, with its own facts.
    let response = http_request(
        address,
        &repositories_get("/api/v1/repositories?clientId=927351842", &member_cookie),
    )
    .await;
    assert_eq!(status_of(&response), "200", "{response}");
    let repositories = response_body(&response)["repositories"]
        .as_array()
        .expect("repositories array")
        .to_owned();
    assert_eq!(repositories.len(), 1, "member sees only beta");
    assert_eq!(
        repositories[0]["repositoryBindingId"],
        beta_binding_id().as_str()
    );
    assert_eq!(repositories[0]["displayName"], "Beta Repo");
    assert_eq!(repositories[0]["defaultBranch"], "trunk");
    assert_eq!(repositories[0]["headCommit"], "b".repeat(64));
    assert_eq!(repositories[0]["dirtyState"], "clean");
    assert_eq!(repositories[0]["availability"], "available");

    // A repository grant without the Client grant is invisible (plan 13.4).
    let response = http_request(
        address,
        &repositories_get("/api/v1/repositories?clientId=927351842", &spectator_cookie),
    )
    .await;
    assert_eq!(status_of(&response), "200", "{response}");
    assert_eq!(
        response_body(&response)["repositories"]
            .as_array()
            .expect("repositories array")
            .len(),
        0,
        "the spectator holds a repository grant but no Client grant"
    );

    running.shutdown().await.expect("shutdown");
    let _ = std::fs::remove_dir_all(&data_directory);
    let _ = std::fs::remove_dir_all(&auth_directory);
}

/// Revalidate evidence: the directory is a live projection of the durable
/// binding facts, so an availability change applied through the repository
/// service (what a device revalidation produces) is consumed by the next
/// directory read without any cache in between.
#[tokio::test]
async fn repository_directory_reflects_revalidated_availability() {
    let data_directory = test_directory("repo-dir-revalidate");
    let auth_directory = test_directory("repo-dir-revalidate-sessions");
    let running = start_server(&data_directory, &auth_directory).await;
    let address = running.local_address();
    let (cookie, owner_id) = initialize_and_login_owner(address).await;

    stage_device(&data_directory);
    upsert_binding(
        &data_directory,
        &alpha_binding_id(),
        "Alpha Repo",
        "main",
        &"a".repeat(40),
        "fingerprint-alpha",
    );
    stage_client_grant(&data_directory, NODE, &owner_id);
    stage_repository_grant(&data_directory, &alpha_binding_id(), &owner_id);

    let read_availability = || async {
        let response = http_request(
            address,
            &repositories_get("/api/v1/repositories?clientId=927351842", &cookie),
        )
        .await;
        assert_eq!(status_of(&response), "200", "{response}");
        let repositories = response_body(&response)["repositories"]
            .as_array()
            .expect("repositories array")
            .to_owned();
        assert_eq!(repositories.len(), 1, "exactly the alpha card");
        (
            repositories[0]["availability"]
                .as_str()
                .expect("availability")
                .to_owned(),
            repositories[0]["dirtyState"]
                .as_str()
                .expect("dirty state")
                .to_owned(),
        )
    };

    assert_eq!(
        read_availability().await,
        ("available".to_owned(), "clean".to_owned()),
        "the launch report projects available"
    );

    // The device reports the directory moved; the projection moves with it.
    let revision = apply_rescan(
        &data_directory,
        &alpha_binding_id(),
        winwincode_storage::RepositoryAvailability::Moved,
        1,
    );
    assert_eq!(revision, 2, "the rescan advances the durable revision");
    assert_eq!(
        read_availability().await,
        ("moved".to_owned(), "dirty".to_owned()),
        "the directory consumes the revalidated availability"
    );

    // A later revalidation restores availability as a dirty work tree.
    let revision = apply_rescan(
        &data_directory,
        &alpha_binding_id(),
        winwincode_storage::RepositoryAvailability::Dirty,
        2,
    );
    assert_eq!(revision, 3);
    assert_eq!(
        read_availability().await,
        ("dirty".to_owned(), "dirty".to_owned()),
        "the directory consumes the second revalidation"
    );

    running.shutdown().await.expect("shutdown");
    let _ = std::fs::remove_dir_all(&data_directory);
    let _ = std::fs::remove_dir_all(&auth_directory);
}
