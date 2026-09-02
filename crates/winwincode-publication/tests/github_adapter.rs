// SPDX-License-Identifier: Apache-2.0

use std::fs;
use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use rusqlite::Connection;
use serde_json::json;
use winwincode_domain::CredentialReferenceId;
use winwincode_publication::test_support::{
    CurrentPublicationCoordinator, current_policy_coordinator, current_publication_fixture,
    current_publication_operations, github_publication_operations_fixture,
};
use winwincode_publication::{
    CredentialResolutionError, GitHubAdapterConfig, GitHubCredential, GitHubCredentialResolver,
    GitHubPublicationAdapter, PublicationOperation, PublicationOperationKind, PublicationPort,
    PublicationPortMutation, PublicationPortObservation, PublicationResourceFact,
    PublicationResourceKind, PublicationSourceIssue, PublicationState, PublicationTarget,
};
use winwincode_storage::{ProductStateStorage, SqliteStorage};

const TOKEN: &str = "fixture-github-token-value";

#[derive(Default)]
struct FixtureCredentialResolver {
    references: Vec<CredentialReferenceId>,
}

impl GitHubCredentialResolver for FixtureCredentialResolver {
    fn resolve(
        &mut self,
        reference: &CredentialReferenceId,
    ) -> Result<GitHubCredential, CredentialResolutionError> {
        self.references.push(reference.clone());
        GitHubCredential::try_new("github", TOKEN)
            .map_err(|_| CredentialResolutionError::unavailable())
    }
}

struct MissingCredentialResolver;

impl GitHubCredentialResolver for MissingCredentialResolver {
    fn resolve(
        &mut self,
        _reference: &CredentialReferenceId,
    ) -> Result<GitHubCredential, CredentialResolutionError> {
        Err(CredentialResolutionError::not_configured())
    }
}

struct OwnedCredentialResolver {
    token: String,
}

impl GitHubCredentialResolver for OwnedCredentialResolver {
    fn resolve(
        &mut self,
        _reference: &CredentialReferenceId,
    ) -> Result<GitHubCredential, CredentialResolutionError> {
        GitHubCredential::try_new("github", self.token.as_bytes())
            .map_err(|_| CredentialResolutionError::unavailable())
    }
}

#[derive(Default)]
struct FixtureState {
    branch_sha: Option<String>,
    pull_request: Option<serde_json::Value>,
    comments: Vec<serde_json::Value>,
    statuses: Vec<serde_json::Value>,
    writes: Vec<String>,
    requests: Vec<FixtureRequest>,
    forced_status: Option<u16>,
    forced_diagnostic: String,
    forced_rate_limit: bool,
    forced_route: Option<String>,
    duplicate_pull_request_once: bool,
    drop_response_after_write_route: Option<String>,
}

struct FixtureRequest {
    method: String,
    path: String,
    authorization: Option<String>,
    body: serde_json::Value,
}

struct FixtureGitHub {
    base_url: String,
    state: Arc<Mutex<FixtureState>>,
    stop: Arc<AtomicBool>,
    thread: Option<JoinHandle<()>>,
}

impl FixtureGitHub {
    fn start() -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind fake GitHub");
        listener
            .set_nonblocking(true)
            .expect("make fake GitHub stoppable");
        let address = listener.local_addr().expect("fake GitHub address");
        let base_url = format!("http://{address}/");
        let state = Arc::new(Mutex::new(FixtureState::default()));
        let stop = Arc::new(AtomicBool::new(false));
        let thread_state = Arc::clone(&state);
        let thread_stop = Arc::clone(&stop);
        let thread = thread::spawn(move || {
            while !thread_stop.load(Ordering::Acquire) {
                match listener.accept() {
                    Ok((stream, _)) => serve_request(stream, &thread_state),
                    Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                        thread::sleep(Duration::from_millis(1));
                    }
                    Err(error) => panic!("fake GitHub accept failed: {error}"),
                }
            }
        });
        Self {
            base_url,
            state,
            stop,
            thread: Some(thread),
        }
    }
}

impl Drop for FixtureGitHub {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Release);
        if let Some(thread) = self.thread.take() {
            let joined = thread.join();
            if !thread::panicking() {
                joined.expect("stop fake GitHub");
            }
        }
    }
}

fn serve_request(mut stream: TcpStream, state: &Arc<Mutex<FixtureState>>) {
    stream
        .set_nonblocking(false)
        .expect("make fake GitHub connection blocking");
    stream
        .set_read_timeout(Some(Duration::from_secs(2)))
        .expect("set request timeout");
    let request = read_request(&mut stream);
    let mut state = state.lock().expect("lock fake GitHub state");
    let route = request.path.split('?').next().expect("request route");
    let route_identity = format!("{} {route}", request.method);
    if let Some(status) = state.forced_status.filter(|_| {
        state
            .forced_route
            .as_ref()
            .is_none_or(|value| value == &route_identity)
    }) {
        let body = json!({ "message": state.forced_diagnostic });
        state.requests.push(request);
        let headers = if state.forced_rate_limit {
            vec![("X-RateLimit-Remaining", "0"), ("Retry-After", "60")]
        } else {
            Vec::new()
        };
        write_response_with_headers(&mut stream, status, &body, &headers);
        return;
    }
    let response = fixture_response(&mut state, &request, route);
    let drop_response = state
        .drop_response_after_write_route
        .as_ref()
        .is_some_and(|value| value == &route_identity);
    if drop_response {
        state.drop_response_after_write_route = None;
    }
    state.requests.push(request);
    if drop_response {
        return;
    }
    write_response(&mut stream, response.0, &response.1);
}

fn fixture_response(
    state: &mut FixtureState,
    request: &FixtureRequest,
    route: &str,
) -> (u16, serde_json::Value) {
    match (request.method.as_str(), route) {
        ("GET", "/repos/example/widget/git/ref/heads/winwincode/delivery") => {
            if let Some(sha) = &state.branch_sha {
                (
                    200,
                    json!({ "object": { "sha": sha }, "url": "fixture:branch" }),
                )
            } else {
                (404, json!({ "message": "not found" }))
            }
        }
        ("POST", "/repos/example/widget/git/refs") => {
            let sha = request.body["sha"]
                .as_str()
                .expect("branch request sha")
                .to_owned();
            state.branch_sha = Some(sha.clone());
            state.writes.push("branch".to_owned());
            (
                201,
                json!({ "object": { "sha": sha }, "url": "fixture:branch" }),
            )
        }
        ("GET", "/repos/example/widget/pulls" | "/repos/example/base/pulls") => (
            200,
            serde_json::Value::Array(state.pull_request.iter().cloned().collect()),
        ),
        ("POST", "/repos/example/widget/pulls") => {
            let pull_request = pull_request_response(
                request,
                "Example/Widget",
                "example/widget",
                "https://github.example/example/widget/pull/17",
            );
            state.pull_request = Some(pull_request.clone());
            if state.duplicate_pull_request_once {
                state.duplicate_pull_request_once = false;
                state.writes.push("pull-request-external-race".to_owned());
                (422, json!({ "message": "already exists" }))
            } else {
                state.writes.push("pull-request".to_owned());
                (201, pull_request)
            }
        }
        ("POST", "/repos/example/base/pulls") => {
            let pull_request = pull_request_response(
                request,
                "example/fork",
                "example/base",
                "https://github.example/example/base/pull/17",
            );
            state.pull_request = Some(pull_request.clone());
            state.writes.push("pull-request".to_owned());
            (201, pull_request)
        }
        ("GET", "/repos/example/widget/issues/7/comments") => {
            (200, serde_json::Value::Array(state.comments.clone()))
        }
        ("POST", "/repos/example/widget/issues/7/comments") => {
            let comment = json!({
                "id": 23,
                "body": request.body["body"],
                "html_url": "https://github.example/example/widget/issues/7#issuecomment-23",
            });
            state.comments.push(comment.clone());
            state.writes.push("issue-comment".to_owned());
            (201, comment)
        }
        ("GET", path)
            if path == format!("/repos/example/widget/commits/{}/statuses", "a".repeat(40)) =>
        {
            (200, serde_json::Value::Array(state.statuses.clone()))
        }
        ("POST", path) if path == format!("/repos/example/widget/statuses/{}", "a".repeat(40)) => {
            let status = json!({
                "id": 31,
                "state": request.body["state"],
                "target_url": request.body["target_url"],
                "description": request.body["description"],
                "context": request.body["context"],
                "url": "https://api.github.example/repos/example/widget/statuses/31",
            });
            state.statuses.push(status.clone());
            state.writes.push("commit-status".to_owned());
            (201, status)
        }
        _ => (404, json!({ "message": "fixture route not found" })),
    }
}

fn pull_request_response(
    request: &FixtureRequest,
    head_repository: &str,
    base_repository: &str,
    html_url: &str,
) -> serde_json::Value {
    json!({
        "number": 17,
        "state": "open",
        "title": request.body["title"],
        "body": request.body["body"],
        "head": {
            "ref": "winwincode/delivery",
            "repo": { "full_name": head_repository },
        },
        "base": {
            "ref": request.body["base"],
            "repo": { "full_name": base_repository },
        },
        "html_url": html_url,
    })
}

fn operation(
    operations: &[PublicationOperation],
    kind: PublicationOperationKind,
) -> &PublicationOperation {
    operations
        .iter()
        .find(|operation| operation.kind() == kind)
        .expect("fixture operation")
}

fn temporary_root() -> PathBuf {
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let nonce = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("clock")
        .as_nanos();
    std::env::temp_dir().join(format!(
        "winwincode-github-adapter-{}-{nonce}-{}",
        std::process::id(),
        COUNTER.fetch_add(1, Ordering::Relaxed),
    ))
}

fn coordinator<'storage, 'port>(
    storage: &'storage mut dyn ProductStateStorage,
    port: &'port mut dyn PublicationPort,
) -> CurrentPublicationCoordinator<'storage, 'port> {
    current_policy_coordinator(storage, port)
}

fn read_request(stream: &mut TcpStream) -> FixtureRequest {
    let mut bytes = Vec::new();
    let mut buffer = [0_u8; 4096];
    let header_end = loop {
        let read = stream.read(&mut buffer).expect("read fake GitHub request");
        assert!(read > 0, "fake GitHub request ended before headers");
        bytes.extend_from_slice(&buffer[..read]);
        if let Some(index) = bytes.windows(4).position(|window| window == b"\r\n\r\n") {
            break index + 4;
        }
    };
    let headers = String::from_utf8(bytes[..header_end].to_vec()).expect("request headers UTF-8");
    let mut lines = headers.split("\r\n");
    let mut request_line = lines.next().expect("request line").split_whitespace();
    let method = request_line.next().expect("request method").to_owned();
    let path = request_line.next().expect("request path").to_owned();
    let mut content_length = 0_usize;
    let mut authorization = None;
    for line in lines {
        let Some((name, value)) = line.split_once(':') else {
            continue;
        };
        if name.eq_ignore_ascii_case("content-length") {
            content_length = value.trim().parse().expect("content length");
        } else if name.eq_ignore_ascii_case("authorization") {
            authorization = Some(value.trim().to_owned());
        }
    }
    while bytes.len() < header_end + content_length {
        let read = stream.read(&mut buffer).expect("read request body");
        assert!(read > 0, "fake GitHub request body ended early");
        bytes.extend_from_slice(&buffer[..read]);
    }
    let body = if content_length == 0 {
        serde_json::Value::Null
    } else {
        serde_json::from_slice(&bytes[header_end..header_end + content_length])
            .expect("request JSON")
    };
    FixtureRequest {
        method,
        path,
        authorization,
        body,
    }
}

fn write_response(stream: &mut TcpStream, status: u16, value: &serde_json::Value) {
    write_response_with_headers(stream, status, value, &[]);
}

fn write_response_with_headers(
    stream: &mut TcpStream,
    status: u16,
    value: &serde_json::Value,
    headers: &[(&str, &str)],
) {
    let body = serde_json::to_vec(value).expect("response JSON");
    let reason = match status {
        200 => "OK",
        201 => "Created",
        404 => "Not Found",
        _ => "Fixture",
    };
    write!(stream, "HTTP/1.1 {status} {reason}\r\n").expect("write status line");
    for (name, value) in headers {
        write!(stream, "{name}: {value}\r\n").expect("write fixture response header");
    }
    write!(
        stream,
        "Content-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
        body.len(),
    )
    .expect("write response headers");
    stream.write_all(&body).expect("write response body");
}

#[test]
fn github_adapter_resolves_the_reference_for_each_request_and_reconciles_one_branch() {
    let github = FixtureGitHub::start();
    let credential_reference = CredentialReferenceId("crd_00000000000000000000000001".to_owned());
    let config =
        GitHubAdapterConfig::try_new(credential_reference.clone(), github.base_url.clone())
            .expect("canonical GitHub adapter config");
    let mut adapter = GitHubPublicationAdapter::new(config, FixtureCredentialResolver::default());
    let branch = current_publication_operations()
        .into_iter()
        .find(|operation| operation.kind() == PublicationOperationKind::Branch)
        .expect("branch operation");

    assert_eq!(
        adapter.lookup(&branch).expect("lookup absent branch"),
        PublicationPortObservation::absent(&branch),
    );
    assert_eq!(
        adapter.apply(&branch).expect("create branch"),
        PublicationPortMutation::applied(&branch, None, true),
    );
    assert_eq!(
        adapter.lookup(&branch).expect("lookup exact branch"),
        PublicationPortObservation::found(&branch, branch.request_sha256(), None),
    );

    let resolver = adapter.into_credential_resolver();
    assert_eq!(
        resolver.references,
        vec![
            credential_reference.clone(),
            credential_reference.clone(),
            credential_reference,
        ],
    );
    let state = github.state.lock().expect("read fake GitHub state");
    assert_eq!(state.requests.len(), 3);
    assert!(
        state
            .requests
            .iter()
            .all(|request| request.authorization.as_deref() == Some(&format!("Bearer {TOKEN}")))
    );
    assert!(!format!("{:?}", state.requests.len()).contains(TOKEN));
}

#[test]
fn github_adapter_applies_and_then_reconciles_the_complete_publication_set() {
    let github = FixtureGitHub::start();
    let config = GitHubAdapterConfig::try_new(
        CredentialReferenceId("crd_00000000000000000000000001".to_owned()),
        github.base_url.clone(),
    )
    .expect("canonical GitHub adapter config");
    let mut adapter = GitHubPublicationAdapter::new(config, FixtureCredentialResolver::default());
    let operations = current_publication_operations();

    for kind in [
        PublicationOperationKind::Branch,
        PublicationOperationKind::PullRequest,
        PublicationOperationKind::IssueComment,
        PublicationOperationKind::CommitStatus,
    ] {
        let operation = operation(&operations, kind);
        assert_eq!(
            adapter.lookup(operation).expect("lookup absent operation"),
            PublicationPortObservation::absent(operation),
        );
        let resource = if kind == PublicationOperationKind::PullRequest {
            Some(
                PublicationResourceFact::try_new(
                    PublicationResourceKind::GitHubPullRequest,
                    "example/widget",
                    17,
                )
                .expect("pull request resource"),
            )
        } else {
            None
        };
        assert_eq!(
            adapter.apply(operation).expect("apply operation"),
            PublicationPortMutation::applied(operation, resource.clone(), true),
        );
        assert_eq!(
            adapter.lookup(operation).expect("lookup exact operation"),
            PublicationPortObservation::found(operation, operation.request_sha256(), resource,),
        );
    }

    let state = github.state.lock().expect("read fake GitHub state");
    assert_eq!(
        state.writes,
        ["branch", "pull-request", "issue-comment", "commit-status"],
    );
    assert!(
        state
            .requests
            .iter()
            .all(|request| request.authorization.as_deref() == Some(&format!("Bearer {TOKEN}")))
    );
}

#[test]
fn github_adapter_classifies_permission_and_rate_limit_without_remote_diagnostics() {
    let github = FixtureGitHub::start();
    let config = GitHubAdapterConfig::try_new(
        CredentialReferenceId("crd_00000000000000000000000001".to_owned()),
        github.base_url.clone(),
    )
    .expect("canonical GitHub adapter config");
    let mut adapter = GitHubPublicationAdapter::new(config, FixtureCredentialResolver::default());
    let branch = current_publication_operations()
        .into_iter()
        .find(|operation| operation.kind() == PublicationOperationKind::Branch)
        .expect("branch operation");

    assert_eq!(
        adapter.lookup(&branch).expect("lookup absent branch"),
        PublicationPortObservation::absent(&branch),
    );
    {
        let mut state = github.state.lock().expect("set permission failure");
        state.forced_status = Some(403);
        state.forced_diagnostic = format!("remote denied credential {TOKEN}");
    }
    assert_eq!(
        adapter.apply(&branch).expect("classify permission failure"),
        PublicationPortMutation::rejected(&branch, "github-permission-denied"),
    );

    {
        let mut state = github.state.lock().expect("set rate limit");
        state.forced_status = Some(429);
        state.forced_rate_limit = true;
    }
    let rate_limited = adapter.lookup(&branch).expect("classify rate limit");
    assert_eq!(
        rate_limited,
        PublicationPortObservation::unknown(&branch, "github-rate-limited"),
    );
    assert!(!format!("{rate_limited:?}").contains(TOKEN));
}

#[test]
fn missing_credential_is_closed_and_makes_no_http_request() {
    let github = FixtureGitHub::start();
    let config = GitHubAdapterConfig::try_new(
        CredentialReferenceId("crd_00000000000000000000000001".to_owned()),
        github.base_url.clone(),
    )
    .expect("canonical GitHub adapter config");
    let mut adapter = GitHubPublicationAdapter::new(config, MissingCredentialResolver);
    let branch = current_publication_operations()
        .into_iter()
        .find(|operation| operation.kind() == PublicationOperationKind::Branch)
        .expect("branch operation");

    let error = adapter
        .lookup(&branch)
        .expect_err("missing credential stops before HTTP");
    assert_eq!(error.code(), "credential-not-configured");
    assert_eq!(
        error.to_string(),
        "publication provider operation did not complete"
    );
    assert!(
        github
            .state
            .lock()
            .expect("read fake GitHub state")
            .requests
            .is_empty()
    );
}

#[test]
fn github_configuration_rejects_credential_bearing_and_non_tls_remote_urls() {
    let credential_reference = CredentialReferenceId("crd_00000000000000000000000001".to_owned());
    for invalid_url in [
        "http://api.github.com",
        "https://token@api.github.com",
        "https://api.github.com?token=secret",
    ] {
        assert!(
            GitHubAdapterConfig::try_new(credential_reference.clone(), invalid_url).is_err(),
            "accepted unsafe GitHub API base URL: {invalid_url}",
        );
    }
    GitHubAdapterConfig::try_new(credential_reference, "http://127.0.0.1:8080")
        .expect("loopback HTTP remains available to the isolated adapter fixture");

    let credential = GitHubCredential::try_new("github", TOKEN).expect("fixture credential");
    let debug = format!("{credential:?}");
    assert!(debug.contains("[REDACTED]"));
    assert!(!debug.contains(TOKEN));
}

#[test]
fn duplicate_pull_request_response_reconciles_the_exact_remote_identity() {
    let github = FixtureGitHub::start();
    let config = GitHubAdapterConfig::try_new(
        CredentialReferenceId("crd_00000000000000000000000001".to_owned()),
        github.base_url.clone(),
    )
    .expect("canonical GitHub adapter config");
    let mut adapter = GitHubPublicationAdapter::new(config, FixtureCredentialResolver::default());
    let pull_request = current_publication_operations()
        .into_iter()
        .find(|operation| operation.kind() == PublicationOperationKind::PullRequest)
        .expect("pull request operation");
    assert_eq!(
        adapter.lookup(&pull_request).expect("lookup absent PR"),
        PublicationPortObservation::absent(&pull_request),
    );
    github
        .state
        .lock()
        .expect("set duplicate PR race")
        .duplicate_pull_request_once = true;
    let resource = PublicationResourceFact::try_new(
        PublicationResourceKind::GitHubPullRequest,
        "example/widget",
        17,
    )
    .expect("PR resource");
    assert_eq!(
        adapter
            .apply(&pull_request)
            .expect("reconcile duplicate PR"),
        PublicationPortMutation::applied(&pull_request, Some(resource.clone()), false),
    );
    assert_eq!(
        adapter.lookup(&pull_request).expect("lookup reconciled PR"),
        PublicationPortObservation::found(
            &pull_request,
            pull_request.request_sha256(),
            Some(resource),
        ),
    );
    let state = github.state.lock().expect("read fake GitHub state");
    assert_eq!(state.writes, ["pull-request-external-race"]);
    assert_eq!(
        state
            .requests
            .iter()
            .filter(|request| {
                request.method == "POST"
                    && request.path.split('?').next() == Some("/repos/example/widget/pulls")
            })
            .count(),
        1,
    );
}

#[test]
fn closed_unrelated_pull_request_on_the_same_route_does_not_own_the_operation() {
    let github = FixtureGitHub::start();
    let config = GitHubAdapterConfig::try_new(
        CredentialReferenceId("crd_00000000000000000000000001".to_owned()),
        github.base_url.clone(),
    )
    .expect("canonical GitHub adapter config");
    let mut adapter = GitHubPublicationAdapter::new(config, FixtureCredentialResolver::default());
    let pull_request = current_publication_operations()
        .into_iter()
        .find(|operation| operation.kind() == PublicationOperationKind::PullRequest)
        .expect("pull request operation");
    github
        .state
        .lock()
        .expect("set historical unrelated PR")
        .pull_request = Some(json!({
        "number": 9,
        "state": "closed",
        "title": "A previous pull request",
        "body": "This PR is not owned by the Publication operation.",
        "head": {
            "ref": "winwincode/delivery",
            "repo": { "full_name": "example/widget" },
        },
        "base": {
            "ref": "main",
            "repo": { "full_name": "example/widget" },
        },
    }));

    assert_eq!(
        adapter
            .lookup(&pull_request)
            .expect("ignore closed unrelated PR"),
        PublicationPortObservation::absent(&pull_request),
    );
    assert!(matches!(
        adapter
            .apply(&pull_request)
            .expect("create the exact marked PR"),
        PublicationPortMutation::Applied {
            remote_write_performed: true,
            ..
        }
    ));
}

#[test]
fn same_organization_fork_pull_request_preserves_both_repository_identities() {
    let github = FixtureGitHub::start();
    let target = PublicationTarget::try_github(
        "example/base",
        "main",
        "example/fork",
        "winwincode/delivery",
    )
    .expect("canonical same-organization fork target");
    let source =
        PublicationSourceIssue::try_github("example/base", 7).expect("canonical source issue");
    let pull_request = github_publication_operations_fixture(target, source, "a".repeat(40))
        .expect("sealed fork operations")
        .into_iter()
        .find(|operation| operation.kind() == PublicationOperationKind::PullRequest)
        .expect("fork pull request operation");
    let config = GitHubAdapterConfig::try_new(
        CredentialReferenceId("crd_00000000000000000000000001".to_owned()),
        github.base_url.clone(),
    )
    .expect("canonical GitHub adapter config");
    let mut adapter = GitHubPublicationAdapter::new(config, FixtureCredentialResolver::default());

    assert_eq!(
        adapter
            .lookup(&pull_request)
            .expect("lookup absent fork PR"),
        PublicationPortObservation::absent(&pull_request),
    );
    assert_eq!(
        adapter.apply(&pull_request).expect("create exact fork PR"),
        PublicationPortMutation::applied(
            &pull_request,
            Some(
                PublicationResourceFact::try_new(
                    PublicationResourceKind::GitHubPullRequest,
                    "example/base",
                    17,
                )
                .expect("fork PR resource"),
            ),
            true,
        ),
    );
    let state = github.state.lock().expect("read fork PR request");
    let request = state
        .requests
        .iter()
        .find(|request| request.method == "POST")
        .expect("fork PR POST");
    assert_eq!(request.body["head"], "example:winwincode/delivery");
    assert_eq!(request.body["head_repo"], "fork");
    assert_eq!(request.body["base"], "main");
}

#[test]
fn lost_create_response_is_reconciled_from_the_durable_operation_after_restart() {
    let root = temporary_root();
    let github = FixtureGitHub::start();
    let fixture = current_publication_fixture();
    let credential_reference = CredentialReferenceId("crd_00000000000000000000000001".to_owned());
    let config =
        GitHubAdapterConfig::try_new(credential_reference.clone(), github.base_url.clone())
            .expect("canonical GitHub adapter config");
    let mut adapter = GitHubPublicationAdapter::new(config, FixtureCredentialResolver::default());
    let mut storage = SqliteStorage::open(&root).expect("open publication storage");
    coordinator(&mut storage, &mut adapter)
        .publish(
            fixture.publish_context(),
            fixture.publish_command(),
            fixture.authorization(),
        )
        .expect("persist publication intent");
    github
        .state
        .lock()
        .expect("set lost branch response")
        .drop_response_after_write_route = Some("POST /repos/example/widget/git/refs".to_owned());

    let unknown = coordinator(&mut storage, &mut adapter)
        .resume(fixture.publication_id(), fixture.resume_time_millis())
        .expect("persist unknown branch result");
    assert_eq!(unknown.state(), PublicationState::Publishing);
    assert_eq!(
        github.state.lock().expect("read GitHub writes").writes,
        ["branch"],
    );
    Box::new(storage).close().expect("close first storage");

    let mut restarted_storage = SqliteStorage::open(&root).expect("restart publication storage");
    let restarted_config =
        GitHubAdapterConfig::try_new(credential_reference, github.base_url.clone())
            .expect("canonical restarted GitHub adapter config");
    let mut restarted_adapter =
        GitHubPublicationAdapter::new(restarted_config, FixtureCredentialResolver::default());
    let published = coordinator(&mut restarted_storage, &mut restarted_adapter)
        .resume(fixture.publication_id(), fixture.resume_time_millis() + 1)
        .expect("reconcile branch and finish publication");
    assert_eq!(published.state(), PublicationState::Published);
    assert_eq!(
        github
            .state
            .lock()
            .expect("read final GitHub writes")
            .writes,
        ["branch", "pull-request", "issue-comment", "commit-status"],
    );

    Box::new(restarted_storage)
        .close()
        .expect("close restarted storage");
    fs::remove_dir_all(&root).expect("remove publication fixture");
}

#[test]
fn rate_limited_publication_resumes_from_durable_requests_without_persisting_the_secret() {
    let root = temporary_root();
    let github = FixtureGitHub::start();
    let fixture = current_publication_fixture();
    let config = GitHubAdapterConfig::try_new(
        CredentialReferenceId("crd_00000000000000000000000001".to_owned()),
        github.base_url.clone(),
    )
    .expect("canonical GitHub adapter config");
    let mut adapter = GitHubPublicationAdapter::new(config, FixtureCredentialResolver::default());
    let mut storage = SqliteStorage::open(&root).expect("open publication storage");

    coordinator(&mut storage, &mut adapter)
        .publish(
            fixture.publish_context(),
            fixture.publish_command(),
            fixture.authorization(),
        )
        .expect("persist complete publication intent before GitHub");
    {
        let mut state = github.state.lock().expect("set GitHub rate limit");
        state.forced_status = Some(429);
        state.forced_rate_limit = true;
        state.forced_diagnostic = format!("remote diagnostic included {TOKEN}");
    }
    let limited = coordinator(&mut storage, &mut adapter)
        .resume(fixture.publication_id(), fixture.resume_time_millis())
        .expect("persist retryable rate limit");
    assert_eq!(limited.state(), PublicationState::Publishing);
    {
        let mut state = github.state.lock().expect("clear GitHub rate limit");
        state.forced_status = None;
        state.forced_rate_limit = false;
        state.forced_diagnostic.clear();
    }
    let published = coordinator(&mut storage, &mut adapter)
        .resume(fixture.publication_id(), fixture.resume_time_millis() + 1)
        .expect("resume from exact durable operations");
    assert_eq!(published.state(), PublicationState::Published);
    assert_eq!(
        published.resource(),
        Some(
            &PublicationResourceFact::try_new(
                PublicationResourceKind::GitHubPullRequest,
                "example/widget",
                17,
            )
            .expect("PR resource"),
        ),
    );
    Box::new(storage)
        .close()
        .expect("close publication storage");

    let database = root.join("control-plane.sqlite3");
    let connection = Connection::open(&database).expect("read durable publication facts");
    let payload: Vec<u8> = connection
        .query_row(
            "SELECT payload FROM product_state WHERE stream_id = ?1",
            [format!("publication:{}", fixture.publication_id().0)],
            |row| row.get(0),
        )
        .expect("read durable publication state");
    assert!(
        !payload
            .windows(TOKEN.len())
            .any(|window| window == TOKEN.as_bytes())
    );
    let mut statement = connection
        .prepare(
            "SELECT payload FROM aggregate_journal_records \
             WHERE aggregate_type = 'publication' AND aggregate_id = ?1 ORDER BY sequence",
        )
        .expect("prepare durable publication journal read");
    let journal_payloads = statement
        .query_map([fixture.publication_id().0.clone()], |row| {
            row.get::<_, Vec<u8>>(0)
        })
        .expect("read durable publication journal")
        .collect::<Result<Vec<_>, _>>()
        .expect("collect durable publication journal");
    assert!(
        journal_payloads
            .iter()
            .any(|record| String::from_utf8_lossy(record).contains("github-rate-limited"))
    );
    assert!(journal_payloads.iter().all(|record| {
        !record
            .windows(TOKEN.len())
            .any(|window| window == TOKEN.as_bytes())
    }));
    let database_bytes = fs::read(&database).expect("read SQLite bytes");
    assert!(
        !database_bytes
            .windows(TOKEN.len())
            .any(|window| window == TOKEN.as_bytes())
    );

    drop(statement);
    drop(connection);
    fs::remove_dir_all(&root).expect("remove publication fixture");
}

#[test]
fn pull_request_permission_failure_never_runs_comment_or_status() {
    let root = temporary_root();
    let github = FixtureGitHub::start();
    let fixture = current_publication_fixture();
    let config = GitHubAdapterConfig::try_new(
        CredentialReferenceId("crd_00000000000000000000000001".to_owned()),
        github.base_url.clone(),
    )
    .expect("canonical GitHub adapter config");
    let mut adapter = GitHubPublicationAdapter::new(config, FixtureCredentialResolver::default());
    let mut storage = SqliteStorage::open(&root).expect("open publication storage");
    coordinator(&mut storage, &mut adapter)
        .publish(
            fixture.publish_context(),
            fixture.publish_command(),
            fixture.authorization(),
        )
        .expect("persist publication intent");
    {
        let mut state = github.state.lock().expect("set pull request denial");
        state.forced_status = Some(403);
        state.forced_route = Some("POST /repos/example/widget/pulls".to_owned());
        state.forced_diagnostic = format!("permission denied for {TOKEN}");
        state.comments.push(json!({
            "id": 99,
            "body": "an unrelated pre-existing comment",
        }));
    }

    let failed = coordinator(&mut storage, &mut adapter)
        .resume(fixture.publication_id(), fixture.resume_time_millis())
        .expect("persist terminal pull request rejection");
    assert_eq!(failed.state(), PublicationState::Failed);
    let state = github.state.lock().expect("read fake GitHub state");
    assert_eq!(state.writes, ["branch"]);
    assert!(state.requests.iter().all(|request| {
        !request.path.contains("/comments") && !request.path.contains("/statuses")
    }));
    assert!(
        !serde_json::to_string(&state.writes)
            .expect("write log JSON")
            .contains(TOKEN)
    );
    drop(state);

    Box::new(storage)
        .close()
        .expect("close publication storage");
    fs::remove_dir_all(&root).expect("remove publication fixture");
}

#[test]
fn comment_rejection_preserves_the_pull_request_identity_and_skips_status() {
    let root = temporary_root();
    let github = FixtureGitHub::start();
    let fixture = current_publication_fixture();
    let config = GitHubAdapterConfig::try_new(
        CredentialReferenceId("crd_00000000000000000000000001".to_owned()),
        github.base_url.clone(),
    )
    .expect("canonical GitHub adapter config");
    let mut adapter = GitHubPublicationAdapter::new(config, FixtureCredentialResolver::default());
    let mut storage = SqliteStorage::open(&root).expect("open publication storage");
    coordinator(&mut storage, &mut adapter)
        .publish(
            fixture.publish_context(),
            fixture.publish_command(),
            fixture.authorization(),
        )
        .expect("persist publication intent");
    {
        let mut state = github.state.lock().expect("set comment denial");
        state.forced_status = Some(403);
        state.forced_route = Some("POST /repos/example/widget/issues/7/comments".to_owned());
        state.forced_diagnostic = format!("comment denied for {TOKEN}");
    }

    let failed = coordinator(&mut storage, &mut adapter)
        .resume(fixture.publication_id(), fixture.resume_time_millis())
        .expect("persist terminal comment rejection");
    assert_eq!(failed.state(), PublicationState::Failed);
    assert_eq!(
        failed.resource(),
        Some(
            &PublicationResourceFact::try_new(
                PublicationResourceKind::GitHubPullRequest,
                "example/widget",
                17,
            )
            .expect("durable pull request identity"),
        ),
    );
    let state = github.state.lock().expect("read fake GitHub state");
    assert_eq!(state.writes, ["branch", "pull-request"]);
    assert!(
        state
            .requests
            .iter()
            .all(|request| !request.path.contains("/statuses/"))
    );
    drop(state);

    Box::new(storage)
        .close()
        .expect("close publication storage");
    fs::remove_dir_all(&root).expect("remove publication fixture");
}

#[test]
fn permission_denied_during_lookup_is_terminal_and_performs_no_remote_write() {
    let root = temporary_root();
    let github = FixtureGitHub::start();
    let fixture = current_publication_fixture();
    let config = GitHubAdapterConfig::try_new(
        CredentialReferenceId("crd_00000000000000000000000001".to_owned()),
        github.base_url.clone(),
    )
    .expect("canonical GitHub adapter config");
    let mut adapter = GitHubPublicationAdapter::new(config, FixtureCredentialResolver::default());
    let mut storage = SqliteStorage::open(&root).expect("open publication storage");
    coordinator(&mut storage, &mut adapter)
        .publish(
            fixture.publish_context(),
            fixture.publish_command(),
            fixture.authorization(),
        )
        .expect("persist publication intent");
    {
        let mut state = github.state.lock().expect("set lookup permission denial");
        state.forced_status = Some(403);
        state.forced_diagnostic = format!("permission denied for {TOKEN}");
    }

    let failed = coordinator(&mut storage, &mut adapter)
        .resume(fixture.publication_id(), fixture.resume_time_millis())
        .expect("persist terminal lookup rejection");
    assert_eq!(failed.state(), PublicationState::Failed);
    let state = github.state.lock().expect("read fake GitHub state");
    assert!(state.writes.is_empty());
    assert_eq!(state.requests.len(), 1);
    assert_eq!(state.requests[0].method, "GET");
    drop(state);

    Box::new(storage)
        .close()
        .expect("close publication storage");
    fs::remove_dir_all(&root).expect("remove publication fixture");
}

#[test]
fn optional_live_github_lane_runs_only_with_explicit_inputs() {
    if std::env::var("WINWINCODE_GITHUB_LIVE_TEST").as_deref() != Ok("1") {
        return;
    }
    let required = |name: &str| {
        std::env::var(name)
            .unwrap_or_else(|_| panic!("{name} is required for the live GitHub lane"))
    };
    let token = required("WINWINCODE_GITHUB_LIVE_TOKEN");
    let repository = required("WINWINCODE_GITHUB_LIVE_REPOSITORY");
    let commit_id = required("WINWINCODE_GITHUB_LIVE_COMMIT");
    let issue_number = required("WINWINCODE_GITHUB_LIVE_ISSUE")
        .parse::<u64>()
        .expect("WINWINCODE_GITHUB_LIVE_ISSUE must be a positive integer");
    let base_branch = required("WINWINCODE_GITHUB_LIVE_BASE_BRANCH");
    let head_branch = required("WINWINCODE_GITHUB_LIVE_HEAD_BRANCH");
    let head_repository = std::env::var("WINWINCODE_GITHUB_LIVE_HEAD_REPOSITORY")
        .unwrap_or_else(|_| repository.clone());
    let target = PublicationTarget::try_github(
        repository.clone(),
        base_branch,
        head_repository,
        head_branch,
    )
    .expect("canonical live pull-request target");
    let source = PublicationSourceIssue::try_github(repository, issue_number)
        .expect("canonical live source issue");
    let operations = github_publication_operations_fixture(target, source, commit_id)
        .expect("sealed live publication operations");
    let config = GitHubAdapterConfig::try_new(
        CredentialReferenceId("crd_00000000000000000000000001".to_owned()),
        "https://api.github.com",
    )
    .expect("canonical live GitHub config");
    let mut adapter = GitHubPublicationAdapter::new(config, OwnedCredentialResolver { token });

    for operation in &operations {
        match adapter.lookup(operation).expect("live GitHub lookup") {
            PublicationPortObservation::Found { .. } => {}
            PublicationPortObservation::Absent { .. } => {
                assert!(matches!(
                    adapter.apply(operation).expect("live GitHub apply"),
                    PublicationPortMutation::Applied { .. }
                ));
            }
            result => panic!("live GitHub operation did not converge: {result:?}"),
        }
    }
    for operation in &operations {
        assert!(matches!(
            adapter
                .lookup(operation)
                .expect("live GitHub replay lookup"),
            PublicationPortObservation::Found { .. }
        ));
    }
}
