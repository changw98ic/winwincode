// SPDX-License-Identifier: Apache-2.0

use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};
use std::time::Duration;

use serde_json::{Value, json};
use winwincode_domain::CredentialReferenceId;
use winwincode_publication::{
    CredentialResolutionError, GitHubCredential, GitHubCredentialResolver,
};

pub const TOKEN: &str = "fixture-github-publication-token";

#[derive(Default)]
pub struct FixtureCredentialResolver;

impl GitHubCredentialResolver for FixtureCredentialResolver {
    fn resolve(
        &mut self,
        _reference: &CredentialReferenceId,
    ) -> Result<GitHubCredential, CredentialResolutionError> {
        GitHubCredential::try_new("github", TOKEN)
            .map_err(|_| CredentialResolutionError::unavailable())
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct FixtureSnapshot {
    pub writes: Vec<String>,
    pub authorizations: Vec<Option<String>>,
}

#[derive(Default)]
struct FixtureState {
    branch_sha: Option<String>,
    pull_request: Option<Value>,
    comments: Vec<Value>,
    statuses: Vec<Value>,
    writes: Vec<String>,
    authorizations: Vec<Option<String>>,
    drop_issue_comment_response_once: bool,
}

pub struct FixtureGitHub {
    pub base_url: String,
    state: Arc<Mutex<FixtureState>>,
    stop: Arc<AtomicBool>,
    thread: Option<JoinHandle<()>>,
}

impl FixtureGitHub {
    pub fn start() -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind fake GitHub");
        listener
            .set_nonblocking(true)
            .expect("make fake GitHub stoppable");
        let address = listener.local_addr().expect("fake GitHub address");
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
            base_url: format!("http://{address}/"),
            state,
            stop,
            thread: Some(thread),
        }
    }

    pub fn drop_issue_comment_response_once(&self) {
        self.state
            .lock()
            .expect("lock fake GitHub state")
            .drop_issue_comment_response_once = true;
    }

    pub fn snapshot(&self) -> FixtureSnapshot {
        let state = self.state.lock().expect("read fake GitHub state");
        FixtureSnapshot {
            writes: state.writes.clone(),
            authorizations: state.authorizations.clone(),
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

struct FixtureRequest {
    method: String,
    path: String,
    authorization: Option<String>,
    body: Value,
}

fn serve_request(mut stream: TcpStream, state: &Arc<Mutex<FixtureState>>) {
    stream
        .set_nonblocking(false)
        .expect("make fake GitHub connection blocking");
    stream
        .set_read_timeout(Some(Duration::from_secs(2)))
        .expect("set fake GitHub read timeout");
    let request = read_request(&mut stream);
    let route = request.path.split('?').next().expect("request route");
    let mut state = state.lock().expect("lock fake GitHub state");
    state.authorizations.push(request.authorization.clone());
    let (status, response, drop_response) = fixture_response(&mut state, &request, route);
    if !drop_response {
        write_response(&mut stream, status, &response);
    }
}

fn fixture_response(
    state: &mut FixtureState,
    request: &FixtureRequest,
    route: &str,
) -> (u16, Value, bool) {
    match (request.method.as_str(), route) {
        ("GET", "/repos/example/widget/git/ref/heads/winwincode/delivery") => {
            state.branch_sha.as_ref().map_or_else(
                || (404, json!({ "message": "not found" }), false),
                |sha| (200, json!({ "object": { "sha": sha } }), false),
            )
        }
        ("POST", "/repos/example/widget/git/refs") => {
            let sha = request.body["sha"]
                .as_str()
                .expect("branch request sha")
                .to_owned();
            state.branch_sha = Some(sha.clone());
            state.writes.push("branch".to_owned());
            (201, json!({ "object": { "sha": sha } }), false)
        }
        ("GET", "/repos/example/widget/pulls") => (
            200,
            Value::Array(state.pull_request.iter().cloned().collect()),
            false,
        ),
        ("POST", "/repos/example/widget/pulls") => {
            let pull_request = json!({
                "number": 17,
                "state": "open",
                "title": request.body["title"],
                "body": request.body["body"],
                "head": {
                    "ref": "winwincode/delivery",
                    "repo": { "full_name": "example/widget" },
                },
                "base": {
                    "ref": request.body["base"],
                    "repo": { "full_name": "example/widget" },
                },
                "html_url": "https://github.example/example/widget/pull/17",
            });
            state.pull_request = Some(pull_request.clone());
            state.writes.push("pull-request".to_owned());
            (201, pull_request, false)
        }
        ("GET", "/repos/example/widget/issues/7/comments") => {
            (200, Value::Array(state.comments.clone()), false)
        }
        ("POST", "/repos/example/widget/issues/7/comments") => {
            let comment = json!({
                "id": 23,
                "body": request.body["body"],
                "html_url": "https://github.example/example/widget/issues/7#issuecomment-23",
            });
            state.comments.push(comment.clone());
            state.writes.push("issue-comment".to_owned());
            let drop_response = state.drop_issue_comment_response_once;
            state.drop_issue_comment_response_once = false;
            (201, comment, drop_response)
        }
        ("GET", path)
            if path == format!("/repos/example/widget/commits/{}/statuses", "a".repeat(40)) =>
        {
            (200, Value::Array(state.statuses.clone()), false)
        }
        ("POST", path) if path == format!("/repos/example/widget/statuses/{}", "a".repeat(40)) => {
            let status = json!({
                "id": 31,
                "state": request.body["state"],
                "target_url": request.body["target_url"],
                "description": request.body["description"],
                "context": request.body["context"],
            });
            state.statuses.push(status.clone());
            state.writes.push("commit-status".to_owned());
            (201, status, false)
        }
        _ => (404, json!({ "message": "fixture route not found" }), false),
    }
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
        Value::Null
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

fn write_response(stream: &mut TcpStream, status: u16, value: &Value) {
    let body = serde_json::to_vec(value).expect("response JSON");
    let reason = match status {
        200 => "OK",
        201 => "Created",
        404 => "Not Found",
        _ => "Fixture",
    };
    write!(
        stream,
        "HTTP/1.1 {status} {reason}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
        body.len(),
    )
    .expect("write fake GitHub response headers");
    stream
        .write_all(&body)
        .expect("write fake GitHub response body");
}
