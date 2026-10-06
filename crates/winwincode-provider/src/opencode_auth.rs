// SPDX-License-Identifier: Apache-2.0

//! `OpenCode` Console device authorization. Secrets stay on the Device.

use std::{
    fmt,
    io::Read,
    sync::atomic::{AtomicBool, Ordering},
    time::{Duration, Instant},
};

use serde::Deserialize;
use serde_json::{Value, json};

use crate::ResolvedSecret;

pub const OPENCODE_ISSUER: &str = "https://opencode.ai/console";
pub const OPENCODE_CLIENT_ID: &str = "opencode-cli";
const MAX_BODY_BYTES: usize = 1024 * 1024;
const MAX_TOKEN_BYTES: usize = 8192;

/// Machine-readable authorization failures. Upstream bodies are never diagnostics.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum OpenCodeAuthError {
    Transport,
    InvalidResponse,
    Http(u16),
    Denied,
    Expired,
    Cancelled,
    Rejected,
}

impl fmt::Display for OpenCodeAuthError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "OpenCode authorization failed: {self:?}")
    }
}
impl std::error::Error for OpenCodeAuthError {}

/// Only the URL, user code and lifetime may enter the authorization projection.
pub struct OpenCodeDeviceGrant {
    pub verification_uri: String,
    pub user_code: String,
    pub expires_in: Duration,
    interval: Duration,
    device_code: ResolvedSecret,
    deadline: Instant,
    next_poll: Instant,
}

/// Access and refresh tokens are Device-private, including their Debug output.
pub struct OpenCodeTokenGrant {
    pub access_token: ResolvedSecret,
    pub refresh_token: ResolvedSecret,
    pub expires_in: Duration,
}

pub enum OpenCodePollResult {
    Pending,
    SlowDown,
    Authorized(OpenCodeTokenGrant),
}

#[derive(Clone, Debug, Deserialize)]
pub struct OpenCodeUser {
    pub id: String,
    pub email: String,
}

#[derive(Clone, Debug, Deserialize)]
pub struct OpenCodeOrganization {
    pub id: String,
    pub name: String,
}

#[derive(Deserialize)]
struct DeviceResponse {
    device_code: String,
    user_code: String,
    verification_uri_complete: String,
    expires_in: u64,
    interval: u64,
}

#[derive(Deserialize)]
struct TokenResponse {
    access_token: String,
    refresh_token: String,
    expires_in: u64,
}

pub struct OpenCodeOAuth {
    http: ureq::Agent,
}

impl Default for OpenCodeOAuth {
    fn default() -> Self {
        Self::new()
    }
}

impl OpenCodeOAuth {
    pub fn new() -> Self {
        let http = ureq::Agent::config_builder()
            .http_status_as_error(false)
            .max_redirects(0)
            .timeout_global(Some(Duration::from_secs(20)))
            .build()
            .into();
        Self { http }
    }

    /// Begins a user-initiated grant using the official client's protocol.
    ///
    /// # Errors
    /// Rejects malformed responses and verification origins outside `OpenCode`.
    pub fn begin(&self) -> Result<OpenCodeDeviceGrant, OpenCodeAuthError> {
        let (status, body) =
            self.post("/auth/device/code", json!({"client_id":OPENCODE_CLIENT_ID}))?;
        require_ok(status)?;
        let response: DeviceResponse = decode(body)?;
        if !valid_text(&response.user_code, 128)
            || response.expires_in == 0
            || response.expires_in > 86400
            || response.interval == 0
            || response.interval > response.expires_in
        {
            return Err(OpenCodeAuthError::InvalidResponse);
        }
        let now = Instant::now();
        let interval = Duration::from_secs(response.interval);
        let expires_in = Duration::from_secs(response.expires_in);
        Ok(OpenCodeDeviceGrant {
            verification_uri: verification_uri(&response.verification_uri_complete)?,
            user_code: response.user_code,
            expires_in,
            interval,
            device_code: secret(response.device_code)?,
            deadline: now + expires_in,
            next_poll: now + interval,
        })
    }

    /// Polls once when due. Calling early does not issue another HTTP request.
    ///
    /// # Errors
    /// Ends cancelled, denied or expired grants. It never retries transport failures.
    pub fn poll_once(
        &self,
        grant: &mut OpenCodeDeviceGrant,
        cancelled: &AtomicBool,
    ) -> Result<OpenCodePollResult, OpenCodeAuthError> {
        if cancelled.load(Ordering::Acquire) {
            return Err(OpenCodeAuthError::Cancelled);
        }
        let now = Instant::now();
        if now >= grant.deadline {
            return Err(OpenCodeAuthError::Expired);
        }
        if now < grant.next_poll {
            return Ok(OpenCodePollResult::Pending);
        }
        let result = self.poll_code(&grant.device_code);
        if cancelled.load(Ordering::Acquire) {
            return Err(OpenCodeAuthError::Cancelled);
        }
        if Instant::now() >= grant.deadline {
            return Err(OpenCodeAuthError::Expired);
        }
        let outcome = result?;
        if matches!(outcome, OpenCodePollResult::SlowDown) {
            grant.interval += Duration::from_secs(5);
        }
        grant.next_poll = Instant::now() + grant.interval;
        Ok(outcome)
    }

    pub(crate) fn poll_code(
        &self,
        device_code: &ResolvedSecret,
    ) -> Result<OpenCodePollResult, OpenCodeAuthError> {
        let (status,body) = self.post("/auth/device/token",json!({
            "client_id":OPENCODE_CLIENT_ID,"grant_type":"urn:ietf:params:oauth:grant-type:device_code",
            "device_code":secret_text(device_code)?,
        }))?;
        decode_poll(status, body)
    }

    /// Reads the Go subscription windows using the inference organization header.
    ///
    /// # Errors
    /// Rejects redirects, invalid organizations, oversized responses and HTTP failures.
    pub fn usage(
        &self,
        access: &ResolvedSecret,
        organization_id: &str,
    ) -> Result<Value, OpenCodeAuthError> {
        if !valid_text(organization_id, 200) {
            return Err(OpenCodeAuthError::InvalidResponse);
        }
        let mut response = self
            .http
            .get(crate::opencode_route::GO_USAGE_ENDPOINT)
            .header("Authorization", format!("Bearer {}", secret_text(access)?))
            .header("x-opencode-org-id", organization_id)
            .header(
                "User-Agent",
                concat!("winwincode/", env!("CARGO_PKG_VERSION")),
            )
            .header("Accept", "application/json")
            .call()
            .map_err(|_| OpenCodeAuthError::Transport)?;
        require_ok(response.status().as_u16())?;
        read_body(&mut response)
    }

    /// Performs one refresh. The caller owns the account lock and durable intent.
    ///
    /// # Errors
    /// An uncertain response must not trigger an automatic retry of the old token.
    pub fn refresh(
        &self,
        refresh: &ResolvedSecret,
    ) -> Result<OpenCodeTokenGrant, OpenCodeAuthError> {
        let (status, body) = self.post("/auth/device/token", json!({
            "client_id":OPENCODE_CLIENT_ID,"grant_type":"refresh_token","refresh_token":secret_text(refresh)?,
        }))?;
        require_ok(status)?;
        decode_token(body)
    }

    /// Reads the actual upstream account identity after authorization.
    ///
    /// # Errors
    /// Rejects invalid or unauthenticated account responses.
    pub fn user(&self, access: &ResolvedSecret) -> Result<OpenCodeUser, OpenCodeAuthError> {
        let user: OpenCodeUser = decode(self.get("/api/user", access, None)?)?;
        if !valid_text(&user.id, 200) || !valid_text(&user.email, 320) {
            return Err(OpenCodeAuthError::InvalidResponse);
        }
        Ok(user)
    }

    /// Reads organization membership. Selection belongs to the user.
    ///
    /// # Errors
    /// Rejects oversized or malformed organization lists.
    pub fn organizations(
        &self,
        access: &ResolvedSecret,
    ) -> Result<Vec<OpenCodeOrganization>, OpenCodeAuthError> {
        let orgs: Vec<OpenCodeOrganization> = decode(self.get("/api/orgs", access, None)?)?;
        if orgs.len() > 100
            || orgs
                .iter()
                .any(|org| !valid_text(&org.id, 200) || !valid_text(&org.name, 200))
        {
            return Err(OpenCodeAuthError::InvalidResponse);
        }
        Ok(orgs)
    }

    /// Reads configuration data only. The caller must validate each inference route.
    ///
    /// # Errors
    /// Rejects invalid organizations and HTTP failures.
    pub fn configuration(
        &self,
        access: &ResolvedSecret,
        org: &str,
    ) -> Result<Value, OpenCodeAuthError> {
        if !valid_text(org, 200) {
            return Err(OpenCodeAuthError::InvalidResponse);
        }
        self.get("/api/config", access, Some(org))
    }

    fn post(&self, path: &str, body: Value) -> Result<(u16, Value), OpenCodeAuthError> {
        let mut response = self
            .http
            .post(format!("{OPENCODE_ISSUER}{path}"))
            .header("Accept", "application/json")
            .header(
                "User-Agent",
                concat!("winwincode/", env!("CARGO_PKG_VERSION")),
            )
            .send_json(body)
            .map_err(|_| OpenCodeAuthError::Transport)?;
        let status = response.status().as_u16();
        Ok((status, read_body(&mut response)?))
    }

    fn get(
        &self,
        path: &str,
        access: &ResolvedSecret,
        org: Option<&str>,
    ) -> Result<Value, OpenCodeAuthError> {
        let mut request = self
            .http
            .get(format!("{OPENCODE_ISSUER}{path}"))
            .header("Accept", "application/json")
            .header(
                "User-Agent",
                concat!("winwincode/", env!("CARGO_PKG_VERSION")),
            )
            .header("Authorization", format!("Bearer {}", secret_text(access)?));
        if let Some(org) = org {
            request = request.header("x-org-id", org);
        }
        let mut response = request.call().map_err(|_| OpenCodeAuthError::Transport)?;
        require_ok(response.status().as_u16())?;
        read_body(&mut response)
    }
}

impl OpenCodeDeviceGrant {
    pub(crate) fn into_private_parts(self) -> (String, String, ResolvedSecret, Duration, Duration) {
        (
            self.verification_uri,
            self.user_code,
            self.device_code,
            self.expires_in,
            self.interval,
        )
    }
}

fn read_body(response: &mut ureq::http::Response<ureq::Body>) -> Result<Value, OpenCodeAuthError> {
    let mut bytes = Vec::new();
    let read = response
        .body_mut()
        .as_reader()
        .take((MAX_BODY_BYTES + 1) as u64)
        .read_to_end(&mut bytes)
        .map_err(|_| OpenCodeAuthError::Transport);
    let parsed = if read.is_err() {
        Err(OpenCodeAuthError::Transport)
    } else if bytes.len() > MAX_BODY_BYTES {
        Err(OpenCodeAuthError::InvalidResponse)
    } else {
        serde_json::from_slice(&bytes).map_err(|_| OpenCodeAuthError::InvalidResponse)
    };
    bytes.fill(0);
    parsed
}

fn decode<T: serde::de::DeserializeOwned>(body: Value) -> Result<T, OpenCodeAuthError> {
    serde_json::from_value(body).map_err(|_| OpenCodeAuthError::InvalidResponse)
}

fn require_ok(status: u16) -> Result<(), OpenCodeAuthError> {
    if status == 200 {
        Ok(())
    } else {
        Err(OpenCodeAuthError::Http(status))
    }
}

fn valid_text(text: &str, limit: usize) -> bool {
    !text.is_empty() && text.len() <= limit && !text.chars().any(char::is_control)
}

fn secret(value: String) -> Result<ResolvedSecret, OpenCodeAuthError> {
    if !valid_text(&value, MAX_TOKEN_BYTES) {
        return Err(OpenCodeAuthError::InvalidResponse);
    }
    ResolvedSecret::from_bytes(value.into_bytes()).map_err(|_| OpenCodeAuthError::InvalidResponse)
}

fn secret_text(secret: &ResolvedSecret) -> Result<&str, OpenCodeAuthError> {
    std::str::from_utf8(secret.expose()).map_err(|_| OpenCodeAuthError::InvalidResponse)
}

fn decode_token(body: Value) -> Result<OpenCodeTokenGrant, OpenCodeAuthError> {
    let token: TokenResponse = decode(body)?;
    if token.expires_in == 0 || token.expires_in > 31_536_000 {
        return Err(OpenCodeAuthError::InvalidResponse);
    }
    Ok(OpenCodeTokenGrant {
        access_token: secret(token.access_token)?,
        refresh_token: secret(token.refresh_token)?,
        expires_in: Duration::from_secs(token.expires_in),
    })
}

fn decode_poll(status: u16, body: Value) -> Result<OpenCodePollResult, OpenCodeAuthError> {
    if status != 200 && status != 400 {
        return Err(OpenCodeAuthError::Http(status));
    }
    match body.get("error").and_then(Value::as_str) {
        Some("authorization_pending") => Ok(OpenCodePollResult::Pending),
        Some("slow_down") => Ok(OpenCodePollResult::SlowDown),
        Some("access_denied") => Err(OpenCodeAuthError::Denied),
        Some("expired_token") => Err(OpenCodeAuthError::Expired),
        Some(_) => Err(OpenCodeAuthError::Rejected),
        None if status == 200 => Ok(OpenCodePollResult::Authorized(decode_token(body)?)),
        None => Err(OpenCodeAuthError::InvalidResponse),
    }
}

pub(crate) fn verification_uri(value: &str) -> Result<String, OpenCodeAuthError> {
    let absolute = if value.starts_with('/') && !value.starts_with("//") {
        format!("https://opencode.ai{value}")
    } else {
        value.to_owned()
    };
    if absolute.len() > 2048
        || absolute.contains(['#', '\\'])
        || absolute
            .bytes()
            .any(|byte| byte.is_ascii_whitespace() || byte.is_ascii_control())
    {
        return Err(OpenCodeAuthError::InvalidResponse);
    }
    let uri: ureq::http::Uri = absolute
        .parse()
        .map_err(|_| OpenCodeAuthError::InvalidResponse)?;
    if uri.scheme_str() != Some("https")
        || !matches!(
            uri.authority().map(ureq::http::uri::Authority::as_str),
            Some("opencode.ai" | "opencode.ai:443")
        )
        || !uri.path().starts_with("/console/")
    {
        return Err(OpenCodeAuthError::InvalidResponse);
    }
    Ok(absolute)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn authorization_rejects_redirect_origins_and_unsafe_tokens() {
        assert!(verification_uri("https://opencode.ai/console/device?user_code=ABCD-EFGH").is_ok());
        assert!(verification_uri("/console/device?user_code=ABCD-EFGH").is_ok());
        for url in [
            "http://opencode.ai/console/device",
            "https://opencode.ai.evil.test/console/device",
            "https://user@opencode.ai/console/device",
            "https://opencode.ai:8443/console/device",
            "//evil.test/console/device",
            "https://opencode.ai/console/device#secret",
        ] {
            assert!(verification_uri(url).is_err());
        }
        for body in [
            json!({"access_token":"","refresh_token":"valid","expires_in":60}),
            json!({"access_token":"valid","refresh_token":"unsafe\nvalue","expires_in":60}),
            json!({"access_token":"valid","refresh_token":"valid","expires_in":0}),
        ] {
            assert!(decode_token(body).is_err());
        }
    }

    #[test]
    fn pending_denied_and_terminal_responses_have_distinct_outcomes() {
        assert!(matches!(
            decode_poll(400, json!({"error":"authorization_pending"})),
            Ok(OpenCodePollResult::Pending)
        ));
        assert!(matches!(
            decode_poll(400, json!({"error":"slow_down"})),
            Ok(OpenCodePollResult::SlowDown)
        ));
        assert!(matches!(
            decode_poll(400, json!({"error":"access_denied"})),
            Err(OpenCodeAuthError::Denied)
        ));
        assert!(matches!(
            decode_poll(400, json!({"error":"expired_token"})),
            Err(OpenCodeAuthError::Expired)
        ));
        assert!(matches!(
            decode_poll(429, json!({"error":"authorization_pending"})),
            Err(OpenCodeAuthError::Http(429))
        ));
        let token = decode_token(json!({"access_token":"private-access","refresh_token":"private-refresh","expires_in":60})).unwrap();
        assert!(!format!("{:?}", token.access_token).contains("private-access"));
    }

    #[test]
    fn early_cancelled_and_expired_polls_never_open_transport() {
        let oauth = OpenCodeOAuth::new();
        let now = Instant::now();
        let mut grant = OpenCodeDeviceGrant {
            verification_uri: String::new(),
            user_code: String::new(),
            expires_in: Duration::from_mins(1),
            interval: Duration::from_secs(5),
            device_code: secret("private-code".into()).unwrap(),
            deadline: now + Duration::from_mins(1),
            next_poll: now + Duration::from_secs(5),
        };
        let cancelled = AtomicBool::new(false);
        assert!(matches!(
            oauth.poll_once(&mut grant, &cancelled),
            Ok(OpenCodePollResult::Pending)
        ));
        cancelled.store(true, Ordering::Release);
        assert!(matches!(
            oauth.poll_once(&mut grant, &cancelled),
            Err(OpenCodeAuthError::Cancelled)
        ));
        cancelled.store(false, Ordering::Release);
        grant.deadline = now;
        assert!(matches!(
            oauth.poll_once(&mut grant, &cancelled),
            Err(OpenCodeAuthError::Expired)
        ));
    }
}
