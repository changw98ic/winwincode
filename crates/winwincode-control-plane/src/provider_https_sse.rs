// SPDX-License-Identifier: Apache-2.0

//! Verified HTTPS Server-Sent Events transport for external model Providers.
//!
//! The adapter borrows a resolved Credential only while opening TLS, stores the
//! authenticated response body rather than the Credential, and converts its
//! bounded Provider-neutral SSE stream through the canonical stream converter.

use std::{
    collections::BTreeMap,
    fmt,
    io::Read as _,
    str::FromStr,
    sync::{Arc, Mutex},
    time::Duration,
};

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use winwincode_domain::{ModelExchangeId, RequestId};

use crate::provider_anthropic::{
    AnthropicCodecErrorKind, AnthropicMessagesOptions, AnthropicToolBindings, ProviderTokenPricing,
    parse_anthropic_sse, prepare_anthropic_request,
};
use crate::{
    CanonicalModelStreamFrame, ModelAttemptFailureFact, ModelExecutionCertainty,
    ProviderAccountAuthorizationPort, ProviderAccountError, ProviderAdapterError,
    ProviderAdapterInvocation, ProviderAdapterOpenReceipt, ProviderAdapterPort,
    ProviderCredentialBundle, ProviderDeviceAuthorization, ProviderDevicePoll,
    ProviderFinishReason, ProviderGatewayOpenReceipt, ProviderGatewayTerminal,
    ProviderStreamControlAction, ProviderStreamConverter, ProviderStreamEvent,
    ProviderStreamFailure, ProviderStreamFailureKind, ProviderTokenUsage, ProviderToolIdentity,
    ProviderToolKind, ResolvedSecret,
};

const MAX_SAFE_INTEGER: u64 = 9_007_199_254_740_991;
const MAX_PROVIDER_ID_BYTES: usize = 128;
const MAX_ENDPOINT_BYTES: usize = 2_048;
const MAX_ADAPTER_REQUEST_ID_BYTES: usize = 200;
const AUTHORIZATION_PREFIX: &[u8] = b"Bearer ";
const CONTROL_PAUSE: u8 = 1;
const CONTROL_RESUME: u8 = 2;
const CONTROL_CANCEL: u8 = 4;
const CONTROL_RELEASE: u8 = 8;
const OPENAI_CHATGPT_RESPONSES_ENDPOINT: &str = "https://chatgpt.com/backend-api/codex/responses";

const OPENAI_AUTH_ISSUER: &str = "https://auth.openai.com";
const OPENAI_AUTH_CLIENT_ID: &str = "app_EMoamEEZ73f0CkXaXp7hrann";
const MAX_DEVICE_AUTH_RESPONSE_BYTES: usize = 64 * 1024;
const OPENAI_DEVICE_LIFETIME_MILLIS: u64 = 15 * 60 * 1_000;

#[derive(Clone)]
pub struct OpenAiDeviceAuthorizationConfig {
    issuer: String,
    client_id: String,
    connect_timeout: Duration,
    response_timeout: Duration,
    total_timeout: Duration,
}

impl OpenAiDeviceAuthorizationConfig {
    /// Creates the production `OpenAI` device-login configuration.
    #[must_use]
    pub fn production() -> Self {
        Self {
            issuer: OPENAI_AUTH_ISSUER.to_owned(),
            client_id: OPENAI_AUTH_CLIENT_ID.to_owned(),
            connect_timeout: Duration::from_secs(5),
            response_timeout: Duration::from_secs(10),
            total_timeout: Duration::from_secs(20),
        }
    }

    /// Replaces the OAuth client identity while retaining the pinned issuer.
    ///
    /// # Errors
    ///
    /// Rejects empty or oversized values.
    pub fn with_client_id(mut self, client_id: String) -> Result<Self, ProviderAccountError> {
        if client_id.trim() != client_id || client_id.is_empty() || client_id.len() > 512 {
            return Err(ProviderAccountError::provider_unavailable());
        }
        self.client_id = client_id;
        Ok(self)
    }

    fn validate(&self) -> Result<(), ProviderAccountError> {
        if self.issuer != OPENAI_AUTH_ISSUER
            || self.client_id.is_empty()
            || self.connect_timeout.is_zero()
            || self.response_timeout.is_zero()
            || self.total_timeout.is_zero()
            || self.connect_timeout > self.total_timeout
            || self.response_timeout > self.total_timeout
        {
            return Err(ProviderAccountError::provider_unavailable());
        }
        Ok(())
    }
}

impl fmt::Debug for OpenAiDeviceAuthorizationConfig {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("OpenAiDeviceAuthorizationConfig")
            .field("issuer", &self.issuer)
            .field("client_id", &"[CONFIGURED]")
            .field("connect_timeout", &self.connect_timeout)
            .field("response_timeout", &self.response_timeout)
            .field("total_timeout", &self.total_timeout)
            .finish()
    }
}

pub struct OpenAiDeviceAuthorizationAdapter {
    config: OpenAiDeviceAuthorizationConfig,
    agent: ureq::Agent,
}

impl OpenAiDeviceAuthorizationAdapter {
    /// Builds a verified, no-proxy `OpenAI` authentication client.
    ///
    /// # Errors
    ///
    /// Rejects invalid authentication configuration.
    pub fn try_new(config: OpenAiDeviceAuthorizationConfig) -> Result<Self, ProviderAccountError> {
        config.validate()?;
        let tls = ureq::tls::TlsConfig::builder()
            .provider(ureq::tls::TlsProvider::Rustls)
            .root_certs(ureq::tls::RootCerts::WebPki)
            .use_sni(true)
            .disable_verification(false)
            .build();
        let agent: ureq::Agent = ureq::Agent::config_builder()
            .http_status_as_error(false)
            .max_redirects(0)
            .proxy(None)
            .timeout_connect(Some(config.connect_timeout))
            .timeout_recv_response(Some(config.response_timeout))
            .timeout_recv_body(Some(config.response_timeout))
            .timeout_global(Some(config.total_timeout))
            .tls_config(tls)
            .build()
            .into();
        Ok(Self { config, agent })
    }

    fn post_json<T: Serialize>(
        &self,
        path: &str,
        input: &T,
    ) -> Result<(u16, Vec<u8>), ProviderAccountError> {
        let body =
            serde_json::to_vec(input).map_err(|_| ProviderAccountError::provider_unavailable())?;
        let response = self
            .agent
            .post(&format!("{}{path}", self.config.issuer))
            .header("Accept", "application/json")
            .header("Content-Type", "application/json")
            .send(&body)
            .map_err(|_| ProviderAccountError::provider_unavailable())?;
        bounded_response(response)
    }

    fn exchange_code(
        &self,
        code: &CodeSuccess,
    ) -> Result<ProviderCredentialBundle, ProviderAccountError> {
        if code.authorization_code.is_empty()
            || code.code_verifier.is_empty()
            || code.authorization_code.len() > 8 * 1024
            || code.code_verifier.len() > 8 * 1024
        {
            return Err(ProviderAccountError::provider_unavailable());
        }
        let body = form_body(&[
            ("grant_type", "authorization_code"),
            ("code", &code.authorization_code),
            (
                "redirect_uri",
                &format!("{}/deviceauth/callback", self.config.issuer),
            ),
            ("client_id", &self.config.client_id),
            ("code_verifier", &code.code_verifier),
        ]);
        let response = self
            .agent
            .post(&format!("{}/oauth/token", self.config.issuer))
            .header("Accept", "application/json")
            .header("Content-Type", "application/x-www-form-urlencoded")
            .send(body.as_bytes())
            .map_err(|_| ProviderAccountError::provider_unavailable())?;
        let (status, body) = bounded_response(response)?;
        if !(200..=299).contains(&status) {
            return Err(ProviderAccountError::provider_unavailable());
        }
        let tokens: TokenResponse = serde_json::from_slice(&body)
            .map_err(|_| ProviderAccountError::provider_unavailable())?;
        ProviderCredentialBundle::from_tokens(tokens.access, tokens.refresh, tokens.identity)
    }
}

impl fmt::Debug for OpenAiDeviceAuthorizationAdapter {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("OpenAiDeviceAuthorizationAdapter")
            .field("config", &self.config)
            .finish_non_exhaustive()
    }
}

impl ProviderAccountAuthorizationPort for OpenAiDeviceAuthorizationAdapter {
    fn start_device_authorization(
        &self,
        now_millis: u64,
    ) -> Result<ProviderDeviceAuthorization, ProviderAccountError> {
        let (status, body) = self.post_json(
            "/api/accounts/deviceauth/usercode",
            &UserCodeRequest {
                client_id: &self.config.client_id,
            },
        )?;
        if !(200..=299).contains(&status) {
            return Err(ProviderAccountError::provider_unavailable());
        }
        let response: UserCodeResponse = serde_json::from_slice(&body)
            .map_err(|_| ProviderAccountError::provider_unavailable())?;
        let interval = response.interval.into_u64()?;
        Ok(ProviderDeviceAuthorization {
            verification_url: format!("{}/codex/device", self.config.issuer),
            user_code: response.user_code,
            device_auth_id: response.device_auth_id,
            poll_after_seconds: interval,
            expires_at_millis: now_millis.saturating_add(OPENAI_DEVICE_LIFETIME_MILLIS),
        })
    }

    fn poll_device_authorization(
        &self,
        device_auth_id: &str,
        user_code: &str,
        _now_millis: u64,
    ) -> Result<ProviderDevicePoll, ProviderAccountError> {
        let (status, body) = self.post_json(
            "/api/accounts/deviceauth/token",
            &TokenPollRequest {
                device_auth_id,
                user_code,
            },
        )?;
        match status {
            200..=299 => {
                let code: CodeSuccess = serde_json::from_slice(&body)
                    .map_err(|_| ProviderAccountError::provider_unavailable())?;
                self.exchange_code(&code)
                    .map(ProviderDevicePoll::Authorized)
            }
            403 | 404 => Ok(ProviderDevicePoll::Pending),
            400 | 401 => Ok(ProviderDevicePoll::Rejected),
            _ => Err(ProviderAccountError::provider_unavailable()),
        }
    }

    fn refresh(
        &self,
        credential: &ProviderCredentialBundle,
        _now_millis: u64,
    ) -> Result<ProviderCredentialBundle, ProviderAccountError> {
        let input = RefreshRequest {
            client_id: &self.config.client_id,
            grant_type: "refresh_token",
            refresh_token: credential.refresh_token(),
        };
        let response = self
            .agent
            .post(&format!("{}/oauth/token", self.config.issuer))
            .header("Accept", "application/json")
            .header("Content-Type", "application/json")
            .send(
                &serde_json::to_vec(&input)
                    .map_err(|_| ProviderAccountError::provider_unavailable())?,
            )
            .map_err(|_| ProviderAccountError::provider_unavailable())?;
        let (status, body) = bounded_response(response)?;
        if !(200..=299).contains(&status) {
            return Err(ProviderAccountError::provider_unavailable());
        }
        let refreshed: RefreshResponse = serde_json::from_slice(&body)
            .map_err(|_| ProviderAccountError::provider_unavailable())?;
        ProviderCredentialBundle::from_tokens(
            refreshed
                .access
                .unwrap_or_else(|| credential.access_token().to_owned()),
            refreshed
                .refresh
                .unwrap_or_else(|| credential.refresh_token().to_owned()),
            refreshed
                .identity
                .unwrap_or_else(|| credential.id_token().to_owned()),
        )
    }

    fn revoke(&self, credential: &ProviderCredentialBundle) -> Result<(), ProviderAccountError> {
        let input = RevokeRequest {
            token: credential.refresh_token(),
            token_type_hint: "refresh_token",
            client_id: &self.config.client_id,
        };
        let (status, _) = self.post_json("/oauth/revoke", &input)?;
        if (200..=299).contains(&status) {
            Ok(())
        } else {
            Err(ProviderAccountError::provider_unavailable())
        }
    }
}

#[derive(Serialize)]
struct UserCodeRequest<'a> {
    client_id: &'a str,
}

#[derive(Deserialize)]
struct UserCodeResponse {
    device_auth_id: String,
    #[serde(alias = "usercode")]
    user_code: String,
    interval: StringOrNumber,
}

#[derive(Deserialize)]
#[serde(untagged)]
enum StringOrNumber {
    String(String),
    Number(u64),
}

impl StringOrNumber {
    fn into_u64(self) -> Result<u64, ProviderAccountError> {
        let value = match self {
            Self::String(value) => value
                .trim()
                .parse()
                .map_err(|_| ProviderAccountError::provider_unavailable())?,
            Self::Number(value) => value,
        };
        if (1..=60).contains(&value) {
            Ok(value)
        } else {
            Err(ProviderAccountError::provider_unavailable())
        }
    }
}

#[derive(Serialize)]
struct TokenPollRequest<'a> {
    device_auth_id: &'a str,
    user_code: &'a str,
}

#[derive(Deserialize)]
struct CodeSuccess {
    authorization_code: String,
    #[serde(rename = "code_challenge")]
    _code_challenge: String,
    code_verifier: String,
}

#[derive(Deserialize)]
struct TokenResponse {
    #[serde(rename = "id_token")]
    identity: String,
    #[serde(rename = "access_token")]
    access: String,
    #[serde(rename = "refresh_token")]
    refresh: String,
}

#[derive(Serialize)]
struct RefreshRequest<'a> {
    client_id: &'a str,
    grant_type: &'static str,
    refresh_token: &'a str,
}

#[derive(Deserialize)]
struct RefreshResponse {
    #[serde(rename = "id_token")]
    identity: Option<String>,
    #[serde(rename = "access_token")]
    access: Option<String>,
    #[serde(rename = "refresh_token")]
    refresh: Option<String>,
}

#[derive(Serialize)]
struct RevokeRequest<'a> {
    token: &'a str,
    token_type_hint: &'static str,
    client_id: &'a str,
}

fn bounded_response(
    response: ureq::http::Response<ureq::Body>,
) -> Result<(u16, Vec<u8>), ProviderAccountError> {
    let status = response.status().as_u16();
    let mut bytes = Vec::new();
    response
        .into_body()
        .into_reader()
        .take((MAX_DEVICE_AUTH_RESPONSE_BYTES + 1) as u64)
        .read_to_end(&mut bytes)
        .map_err(|_| ProviderAccountError::provider_unavailable())?;
    if bytes.len() > MAX_DEVICE_AUTH_RESPONSE_BYTES {
        return Err(ProviderAccountError::provider_unavailable());
    }
    Ok((status, bytes))
}

fn form_body(fields: &[(&str, &str)]) -> String {
    fields
        .iter()
        .map(|(name, value)| format!("{}={}", form_component(name), form_component(value)))
        .collect::<Vec<_>>()
        .join("&")
}

fn form_component(value: &str) -> String {
    const HEX: &[u8; 16] = b"0123456789ABCDEF";
    let mut encoded = String::with_capacity(value.len());
    for byte in value.bytes() {
        if byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'.' | b'_' | b'~') {
            encoded.push(char::from(byte));
        } else {
            encoded.push('%');
            encoded.push(char::from(HEX[usize::from(byte >> 4)]));
            encoded.push(char::from(HEX[usize::from(byte & 0x0f)]));
        }
    }
    encoded
}

/// TLS trust used by one external Provider endpoint.
#[derive(Clone)]
pub enum ProviderTlsRoots {
    /// Mozilla `WebPKI` roots shipped by the pinned HTTP stack.
    WebPki,
    /// Explicit DER roots used by private deployments and deterministic TLS gates.
    Specific(Vec<Vec<u8>>),
}

/// All transport deadlines applied to one HTTPS/SSE exchange.
#[derive(Clone, Copy, Debug)]
pub struct HttpsSseProviderTimeouts {
    pub connect: Duration,
    pub first_byte: Duration,
    pub idle: Duration,
    pub total: Duration,
}

/// Hard memory and event-count bounds for one HTTPS/SSE exchange.
#[derive(Clone, Copy, Debug)]
pub struct HttpsSseProviderLimits {
    pub response_bytes: usize,
    pub event_bytes: usize,
    pub events: usize,
}

impl fmt::Debug for ProviderTlsRoots {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::WebPki => formatter.write_str("ProviderTlsRoots::WebPki"),
            Self::Specific(values) => formatter
                .debug_tuple("ProviderTlsRoots::Specific")
                .field(&values.len())
                .finish(),
        }
    }
}

/// Bounded HTTPS/SSE transport configuration.
#[derive(Clone, Debug)]
pub struct HttpsSseProviderConfig {
    provider_id: String,
    endpoint: String,
    connect_timeout: Duration,
    first_byte_timeout: Duration,
    idle_timeout: Duration,
    total_timeout: Duration,
    max_response_bytes: usize,
    max_event_bytes: usize,
    max_events: usize,
    tls_roots: ProviderTlsRoots,
    protocol: HttpsSseProviderProtocol,
}

#[derive(Clone, Copy, Debug)]
enum HttpsSseProviderProtocol {
    Canonical,
    AnthropicMessages(AnthropicMessagesOptions),
    OpenAiChatGptResponses,
}

impl HttpsSseProviderConfig {
    /// Creates one WebPKI-verified external Provider configuration.
    ///
    /// # Errors
    ///
    /// Rejects non-HTTPS/credential-bearing endpoints and unsafe timeout or
    /// response limits.
    pub fn try_new(
        provider_id: String,
        endpoint: String,
        timeouts: HttpsSseProviderTimeouts,
        limits: HttpsSseProviderLimits,
    ) -> Result<Self, HttpsSseProviderError> {
        let config = Self {
            provider_id,
            endpoint,
            connect_timeout: timeouts.connect,
            first_byte_timeout: timeouts.first_byte,
            idle_timeout: timeouts.idle,
            total_timeout: timeouts.total,
            max_response_bytes: limits.response_bytes,
            max_event_bytes: limits.event_bytes,
            max_events: limits.events,
            tls_roots: ProviderTlsRoots::WebPki,
            protocol: HttpsSseProviderProtocol::Canonical,
        };
        config.validate()?;
        Ok(config)
    }

    /// Replaces `WebPKI` roots with an explicit non-empty DER trust set.
    ///
    /// # Errors
    ///
    /// Rejects empty certificates, empty sets, or excessive certificate bytes.
    pub fn with_specific_tls_roots(
        mut self,
        roots: Vec<Vec<u8>>,
    ) -> Result<Self, HttpsSseProviderError> {
        if roots.is_empty()
            || roots.len() > 32
            || roots
                .iter()
                .any(|root| root.is_empty() || root.len() > 64 * 1024)
        {
            return Err(HttpsSseProviderError::new(
                HttpsSseProviderErrorKind::InvalidConfiguration,
            ));
        }
        self.tls_roots = ProviderTlsRoots::Specific(roots);
        self.validate()?;
        Ok(self)
    }

    /// Selects the Anthropic Messages request and streaming protocol.
    ///
    /// The configured Provider route remains authoritative for the exact
    /// upstream model ID. Local display annotations such as `[1m]` must not be
    /// included in that ID.
    ///
    /// # Errors
    ///
    /// Rejects zero output limits or unsafe token pricing.
    pub fn with_anthropic_messages(
        mut self,
        max_output_tokens: u32,
        pricing: ProviderTokenPricing,
    ) -> Result<Self, HttpsSseProviderError> {
        let options = AnthropicMessagesOptions {
            max_output_tokens,
            pricing,
        };
        options.validate().map_err(map_anthropic_configuration)?;
        self.protocol = HttpsSseProviderProtocol::AnthropicMessages(options);
        self.validate()?;
        Ok(self)
    }

    /// Selects the `ChatGPT`-authenticated `OpenAI` Responses protocol.
    ///
    /// The protocol is intentionally pinned to the production `ChatGPT` Codex
    /// endpoint so an account credential cannot be redirected by configuration.
    ///
    /// # Errors
    ///
    /// Rejects any endpoint other than the pinned `ChatGPT` Responses endpoint.
    pub fn with_openai_chatgpt_responses(mut self) -> Result<Self, HttpsSseProviderError> {
        self.protocol = HttpsSseProviderProtocol::OpenAiChatGptResponses;
        self.validate()?;
        Ok(self)
    }

    fn validate(&self) -> Result<(), HttpsSseProviderError> {
        if !valid_token(&self.provider_id, MAX_PROVIDER_ID_BYTES)
            || self.endpoint.len() > MAX_ENDPOINT_BYTES
            || !canonical_https_endpoint(&self.endpoint)
            || self.connect_timeout.is_zero()
            || self.first_byte_timeout.is_zero()
            || self.idle_timeout.is_zero()
            || self.total_timeout.is_zero()
            || self.connect_timeout > self.total_timeout
            || self.first_byte_timeout > self.total_timeout
            || self.idle_timeout > self.total_timeout
            || self.max_event_bytes == 0
            || self.max_response_bytes < self.max_event_bytes
            || self.max_response_bytes > 64 * 1024 * 1024
            || self.max_events == 0
            || self.max_events > 100_000
        {
            return Err(HttpsSseProviderError::new(
                HttpsSseProviderErrorKind::InvalidConfiguration,
            ));
        }
        if let HttpsSseProviderProtocol::AnthropicMessages(options) = self.protocol {
            options.validate().map_err(map_anthropic_configuration)?;
        }
        if matches!(
            self.protocol,
            HttpsSseProviderProtocol::OpenAiChatGptResponses
        ) && self.endpoint != OPENAI_CHATGPT_RESPONSES_ENDPOINT
        {
            return Err(HttpsSseProviderError::new(
                HttpsSseProviderErrorKind::InvalidConfiguration,
            ));
        }
        Ok(())
    }

    #[must_use]
    pub fn provider_id(&self) -> &str {
        &self.provider_id
    }
}

/// Stable transport failure categories without upstream response text.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum HttpsSseProviderErrorKind {
    InvalidConfiguration,
    IdentityConflict,
    RateLimited,
    Rejected,
    Unavailable,
    Transport,
    Protocol,
    SizeLimit,
    Paused,
    CredentialLeak,
}

/// Secret-free HTTPS/SSE error.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct HttpsSseProviderError {
    kind: HttpsSseProviderErrorKind,
}

impl HttpsSseProviderError {
    const fn new(kind: HttpsSseProviderErrorKind) -> Self {
        Self { kind }
    }

    #[must_use]
    pub const fn kind(&self) -> HttpsSseProviderErrorKind {
        self.kind
    }

    /// Produces the stable terminal fact used when an accepted stream breaks.
    #[must_use]
    pub const fn failure_terminal(&self) -> ProviderGatewayTerminal {
        let failure_kind = match self.kind {
            HttpsSseProviderErrorKind::RateLimited => ModelAttemptFailureFact {
                kind: crate::ModelAttemptFailureKind::RateLimit,
                certainty: ModelExecutionCertainty::RejectedBeforeAcceptance,
            },
            HttpsSseProviderErrorKind::Rejected => ModelAttemptFailureFact {
                kind: crate::ModelAttemptFailureKind::InvalidRequest,
                certainty: ModelExecutionCertainty::RejectedBeforeAcceptance,
            },
            HttpsSseProviderErrorKind::Protocol
            | HttpsSseProviderErrorKind::SizeLimit
            | HttpsSseProviderErrorKind::CredentialLeak => ModelAttemptFailureFact {
                kind: crate::ModelAttemptFailureKind::Protocol,
                certainty: ModelExecutionCertainty::AcceptanceUnknown,
            },
            HttpsSseProviderErrorKind::InvalidConfiguration
            | HttpsSseProviderErrorKind::IdentityConflict
            | HttpsSseProviderErrorKind::Unavailable
            | HttpsSseProviderErrorKind::Transport
            | HttpsSseProviderErrorKind::Paused => ModelAttemptFailureFact {
                kind: crate::ModelAttemptFailureKind::Transport,
                certainty: ModelExecutionCertainty::AcceptanceUnknown,
            },
        };
        ProviderGatewayTerminal::Failed {
            failure: failure_kind,
            charge: None,
        }
    }
}

impl fmt::Display for HttpsSseProviderError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("external Provider HTTPS/SSE operation failed")
    }
}

impl std::error::Error for HttpsSseProviderError {}

/// Canonical frames and terminal facts drained from one verified SSE response.
pub struct HttpsSseProviderCompletion {
    pub frames: Vec<CanonicalModelStreamFrame>,
    pub terminal: ProviderGatewayTerminal,
}

impl fmt::Debug for HttpsSseProviderCompletion {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("HttpsSseProviderCompletion")
            .field("frame_count", &self.frames.len())
            .field("terminal", &self.terminal.outcome())
            .finish()
    }
}

/// External Provider adapter using pinned rustls verification and bounded SSE.
#[derive(Clone)]
pub struct HttpsSseProviderAdapter {
    shared: Arc<SharedAdapter>,
}

struct SharedAdapter {
    config: HttpsSseProviderConfig,
    agent: ureq::Agent,
    streams: Mutex<BTreeMap<String, StreamRecord>>,
}

struct StreamRecord {
    model_exchange_id: ModelExchangeId,
    invocation_digest: [u8; 32],
    controls: u8,
    tool_bindings: AnthropicToolBindings,
    state: StreamState,
}

#[derive(Clone, Copy)]
struct HttpInvocation<'a> {
    model_exchange_id: &'a ModelExchangeId,
    request_id: &'a RequestId,
    adapter_request_id: &'a str,
    model_id: &'a str,
    content_type: &'a str,
    payload: &'a [u8],
}

impl<'a> HttpInvocation<'a> {
    fn from_adapter(invocation: &'a ProviderAdapterInvocation<'_>) -> Self {
        Self {
            model_exchange_id: invocation.model_exchange_id(),
            request_id: invocation.request_id(),
            adapter_request_id: invocation.adapter_request_id(),
            model_id: invocation.model_id(),
            content_type: invocation.content_type(),
            payload: invocation.payload(),
        }
    }
}

enum StreamState {
    Pending,
    Open(Option<ureq::Body>),
    Drained,
    Cancelled,
    Released,
    Fenced,
}

impl HttpsSseProviderAdapter {
    /// Builds a verified no-proxy HTTP agent from one validated configuration.
    ///
    /// # Errors
    ///
    /// Rejects malformed explicit TLS roots or transport configuration.
    pub fn try_new(config: HttpsSseProviderConfig) -> Result<Self, HttpsSseProviderError> {
        config.validate()?;
        let root_certs = match &config.tls_roots {
            ProviderTlsRoots::WebPki => ureq::tls::RootCerts::WebPki,
            ProviderTlsRoots::Specific(values) => values
                .iter()
                .map(|value| ureq::tls::Certificate::from_der(value).to_owned())
                .collect::<Vec<_>>()
                .into(),
        };
        let tls = ureq::tls::TlsConfig::builder()
            .provider(ureq::tls::TlsProvider::Rustls)
            .root_certs(root_certs)
            .use_sni(true)
            .disable_verification(false)
            .build();
        let agent: ureq::Agent = ureq::Agent::config_builder()
            .http_status_as_error(false)
            .max_redirects(0)
            .proxy(None)
            .timeout_connect(Some(config.connect_timeout))
            .timeout_recv_response(Some(config.first_byte_timeout))
            .timeout_recv_body(Some(config.idle_timeout))
            .timeout_global(Some(config.total_timeout))
            .tls_config(tls)
            .build()
            .into();
        Ok(Self {
            shared: Arc::new(SharedAdapter {
                config,
                agent,
                streams: Mutex::new(BTreeMap::new()),
            }),
        })
    }

    /// Drains the accepted response once and converts it through the unique
    /// canonical stream converter carried by the Gateway receipt.
    ///
    /// # Errors
    ///
    /// Rejects foreign identities, paused/replayed drains, TLS/body failures,
    /// malformed or oversized SSE, and Credential leakage.
    pub fn drain_canonical(
        &self,
        receipt: &ProviderGatewayOpenReceipt,
    ) -> Result<HttpsSseProviderCompletion, HttpsSseProviderError> {
        if receipt.route.provider_id != self.shared.config.provider_id {
            return Err(HttpsSseProviderError::new(
                HttpsSseProviderErrorKind::IdentityConflict,
            ));
        }
        let mut body = self.take_body(receipt)?;
        let tool_bindings = self.stream_tool_bindings(receipt)?;
        let result = self.convert_response(receipt, &mut body, &tool_bindings);
        self.finish_drain(receipt)?;
        result
    }

    fn convert_response(
        &self,
        receipt: &ProviderGatewayOpenReceipt,
        body: &mut ureq::Body,
        tool_bindings: &AnthropicToolBindings,
    ) -> Result<HttpsSseProviderCompletion, HttpsSseProviderError> {
        let bytes = self.read_bounded(receipt, body)?;
        let (events, terminal) = match self.shared.config.protocol {
            HttpsSseProviderProtocol::Canonical => {
                let parsed = parse_sse(
                    &bytes,
                    self.shared.config.max_event_bytes,
                    self.shared.config.max_events,
                )?;
                (parsed.events, parsed.terminal)
            }
            HttpsSseProviderProtocol::AnthropicMessages(options) => {
                let parsed = parse_anthropic_sse(
                    &bytes,
                    self.shared.config.max_event_bytes,
                    self.shared.config.max_events,
                    tool_bindings,
                    options,
                )
                .map_err(map_anthropic_response)?;
                (parsed.events, parsed.terminal)
            }
            HttpsSseProviderProtocol::OpenAiChatGptResponses => {
                let parsed = parse_openai_responses_sse(
                    &bytes,
                    self.shared.config.max_event_bytes,
                    self.shared.config.max_events,
                )?;
                (parsed.events, parsed.terminal)
            }
        };
        let mut converter = ProviderStreamConverter::from_gateway_receipt(receipt);
        let mut frames = Vec::new();
        for event in events {
            frames.extend(converter.ingest(event).map_err(|error| {
                if error.kind() == crate::ProviderStreamConversionErrorKind::CredentialLeak {
                    HttpsSseProviderError::new(HttpsSseProviderErrorKind::CredentialLeak)
                } else {
                    HttpsSseProviderError::new(HttpsSseProviderErrorKind::Protocol)
                }
            })?);
        }
        Ok(HttpsSseProviderCompletion { frames, terminal })
    }

    fn stream_tool_bindings(
        &self,
        receipt: &ProviderGatewayOpenReceipt,
    ) -> Result<AnthropicToolBindings, HttpsSseProviderError> {
        let streams = self
            .shared
            .streams
            .lock()
            .map_err(|_| HttpsSseProviderError::new(HttpsSseProviderErrorKind::Unavailable))?;
        let record = streams.get(&receipt.adapter_request_id).ok_or_else(|| {
            HttpsSseProviderError::new(HttpsSseProviderErrorKind::IdentityConflict)
        })?;
        if record.model_exchange_id != receipt.model_exchange_id {
            return Err(HttpsSseProviderError::new(
                HttpsSseProviderErrorKind::IdentityConflict,
            ));
        }
        Ok(record.tool_bindings.clone())
    }

    fn read_bounded(
        &self,
        receipt: &ProviderGatewayOpenReceipt,
        body: &mut ureq::Body,
    ) -> Result<Vec<u8>, HttpsSseProviderError> {
        let mut reader = body.as_reader();
        let mut bytes = Vec::new();
        let mut buffer = [0_u8; 8 * 1024];
        loop {
            if self.drain_interrupted(receipt)? {
                return Err(HttpsSseProviderError::new(
                    HttpsSseProviderErrorKind::Transport,
                ));
            }
            let read = reader
                .read(&mut buffer)
                .map_err(|_| HttpsSseProviderError::new(HttpsSseProviderErrorKind::Transport))?;
            if read == 0 {
                return Ok(bytes);
            }
            if bytes.len().saturating_add(read) > self.shared.config.max_response_bytes {
                return Err(HttpsSseProviderError::new(
                    HttpsSseProviderErrorKind::SizeLimit,
                ));
            }
            bytes.extend_from_slice(&buffer[..read]);
        }
    }

    fn drain_interrupted(
        &self,
        receipt: &ProviderGatewayOpenReceipt,
    ) -> Result<bool, HttpsSseProviderError> {
        let streams = self
            .shared
            .streams
            .lock()
            .map_err(|_| HttpsSseProviderError::new(HttpsSseProviderErrorKind::Unavailable))?;
        let record = streams.get(&receipt.adapter_request_id).ok_or_else(|| {
            HttpsSseProviderError::new(HttpsSseProviderErrorKind::IdentityConflict)
        })?;
        if record.model_exchange_id != receipt.model_exchange_id {
            return Err(HttpsSseProviderError::new(
                HttpsSseProviderErrorKind::IdentityConflict,
            ));
        }
        Ok(matches!(
            record.state,
            StreamState::Cancelled | StreamState::Released | StreamState::Fenced
        ))
    }

    fn take_body(
        &self,
        receipt: &ProviderGatewayOpenReceipt,
    ) -> Result<ureq::Body, HttpsSseProviderError> {
        let mut streams = self
            .shared
            .streams
            .lock()
            .map_err(|_| HttpsSseProviderError::new(HttpsSseProviderErrorKind::Unavailable))?;
        let record = streams
            .get_mut(&receipt.adapter_request_id)
            .ok_or_else(|| {
                HttpsSseProviderError::new(HttpsSseProviderErrorKind::IdentityConflict)
            })?;
        if record.model_exchange_id != receipt.model_exchange_id {
            return Err(HttpsSseProviderError::new(
                HttpsSseProviderErrorKind::IdentityConflict,
            ));
        }
        match &mut record.state {
            StreamState::Open(body) if record.controls & CONTROL_PAUSE == 0 => body
                .take()
                .ok_or_else(|| HttpsSseProviderError::new(HttpsSseProviderErrorKind::Transport)),
            StreamState::Open(_) => Err(HttpsSseProviderError::new(
                HttpsSseProviderErrorKind::Paused,
            )),
            StreamState::Pending => Err(HttpsSseProviderError::new(
                HttpsSseProviderErrorKind::Unavailable,
            )),
            StreamState::Drained
            | StreamState::Cancelled
            | StreamState::Released
            | StreamState::Fenced => Err(HttpsSseProviderError::new(
                HttpsSseProviderErrorKind::IdentityConflict,
            )),
        }
    }

    fn finish_drain(
        &self,
        receipt: &ProviderGatewayOpenReceipt,
    ) -> Result<(), HttpsSseProviderError> {
        let mut streams = self
            .shared
            .streams
            .lock()
            .map_err(|_| HttpsSseProviderError::new(HttpsSseProviderErrorKind::Unavailable))?;
        let record = streams
            .get_mut(&receipt.adapter_request_id)
            .ok_or_else(|| {
                HttpsSseProviderError::new(HttpsSseProviderErrorKind::IdentityConflict)
            })?;
        match record.state {
            StreamState::Open(None) => {
                record.state = StreamState::Drained;
                Ok(())
            }
            StreamState::Cancelled | StreamState::Released => Ok(()),
            StreamState::Pending
            | StreamState::Open(Some(_))
            | StreamState::Drained
            | StreamState::Fenced => Err(HttpsSseProviderError::new(
                HttpsSseProviderErrorKind::IdentityConflict,
            )),
        }
    }

    fn existing_open(
        &self,
        invocation: HttpInvocation<'_>,
        digest: &[u8; 32],
    ) -> Result<Option<ProviderAdapterOpenReceipt>, ProviderAdapterError> {
        let streams = self
            .shared
            .streams
            .lock()
            .map_err(|_| ProviderAdapterError::unavailable())?;
        let Some(record) = streams.get(invocation.adapter_request_id) else {
            return Ok(None);
        };
        if record.model_exchange_id != *invocation.model_exchange_id {
            return Err(ProviderAdapterError::protocol());
        }
        if matches!(
            record.state,
            StreamState::Cancelled | StreamState::Released | StreamState::Fenced
        ) {
            return Err(ProviderAdapterError::rejected());
        }
        if record.invocation_digest != *digest {
            return Err(ProviderAdapterError::protocol());
        }
        match record.state {
            StreamState::Open(_) | StreamState::Drained => {
                ProviderAdapterOpenReceipt::try_new(invocation.adapter_request_id.to_owned())
                    .map(Some)
            }
            StreamState::Pending => Err(ProviderAdapterError::unavailable()),
            StreamState::Cancelled | StreamState::Released | StreamState::Fenced => {
                unreachable!("terminal controls returned before digest validation")
            }
        }
    }

    fn begin_open(
        &self,
        invocation: HttpInvocation<'_>,
        digest: [u8; 32],
        tool_bindings: AnthropicToolBindings,
    ) -> Result<(), ProviderAdapterError> {
        let mut streams = self
            .shared
            .streams
            .lock()
            .map_err(|_| ProviderAdapterError::unavailable())?;
        if streams.contains_key(invocation.adapter_request_id) {
            return Err(ProviderAdapterError::unavailable());
        }
        streams.insert(
            invocation.adapter_request_id.to_owned(),
            StreamRecord {
                model_exchange_id: invocation.model_exchange_id.clone(),
                invocation_digest: digest,
                controls: 0,
                tool_bindings,
                state: StreamState::Pending,
            },
        );
        Ok(())
    }

    fn finish_open(
        &self,
        invocation: HttpInvocation<'_>,
        state: StreamState,
    ) -> Result<(), ProviderAdapterError> {
        let mut streams = self
            .shared
            .streams
            .lock()
            .map_err(|_| ProviderAdapterError::unavailable())?;
        let record = streams
            .get_mut(invocation.adapter_request_id)
            .ok_or_else(ProviderAdapterError::unavailable)?;
        if record.model_exchange_id != *invocation.model_exchange_id {
            return Err(ProviderAdapterError::protocol());
        }
        match record.state {
            StreamState::Pending => {
                record.state = state;
                Ok(())
            }
            StreamState::Cancelled | StreamState::Released | StreamState::Fenced => {
                Err(ProviderAdapterError::rejected())
            }
            StreamState::Open(_) | StreamState::Drained => Err(ProviderAdapterError::protocol()),
        }
    }

    fn abandon_retryable_open(
        &self,
        invocation: HttpInvocation<'_>,
    ) -> Result<(), ProviderAdapterError> {
        let mut streams = self
            .shared
            .streams
            .lock()
            .map_err(|_| ProviderAdapterError::unavailable())?;
        let is_exact_pending = streams
            .get(invocation.adapter_request_id)
            .is_some_and(|record| {
                record.model_exchange_id == *invocation.model_exchange_id
                    && matches!(record.state, StreamState::Pending)
            });
        if is_exact_pending {
            streams.remove(invocation.adapter_request_id);
            Ok(())
        } else {
            Err(ProviderAdapterError::protocol())
        }
    }

    #[allow(clippy::too_many_lines)] // Keeps transport fencing and response ownership in one path.
    fn open_https(
        &self,
        invocation: HttpInvocation<'_>,
        credential: &[u8],
    ) -> Result<ProviderAdapterOpenReceipt, ProviderAdapterError> {
        let digest = invocation_digest(invocation);
        if let Some(replay) = self.existing_open(invocation, &digest)? {
            return Ok(replay);
        }
        let anthropic_request = match self.shared.config.protocol {
            HttpsSseProviderProtocol::AnthropicMessages(options) => Some(
                prepare_anthropic_request(invocation.payload, invocation.model_id, options)
                    .map_err(map_anthropic_request)?,
            ),
            HttpsSseProviderProtocol::Canonical
            | HttpsSseProviderProtocol::OpenAiChatGptResponses => None,
        };
        let openai_request = if matches!(
            self.shared.config.protocol,
            HttpsSseProviderProtocol::OpenAiChatGptResponses
        ) {
            Some(prepare_openai_responses_request(
                invocation.payload,
                invocation.model_id,
            )?)
        } else {
            None
        };
        let tool_bindings = anthropic_request
            .as_ref()
            .map_or_else(AnthropicToolBindings::default, |request| {
                request.tool_bindings.clone()
            });
        self.begin_open(invocation, digest, tool_bindings)?;
        let authentication = match self.shared.config.protocol {
            HttpsSseProviderProtocol::OpenAiChatGptResponses => openai_authentication(credential),
            HttpsSseProviderProtocol::Canonical
            | HttpsSseProviderProtocol::AnthropicMessages(_) => authorization_value(credential)
                .map(|authorization| OpenAiRequestAuthentication {
                    authorization,
                    account_id: None,
                }),
        }
        .inspect_err(|_error| {
            let _ = self.finish_open(invocation, StreamState::Fenced);
        })?;
        let mut request = self
            .shared
            .agent
            .post(&self.shared.config.endpoint)
            .header("Accept", "text/event-stream")
            .header("Authorization", &authentication.authorization)
            .header("Idempotency-Key", invocation.adapter_request_id);
        let payload = match self.shared.config.protocol {
            HttpsSseProviderProtocol::Canonical => {
                request = request
                    .header("Content-Type", invocation.content_type)
                    .header("X-WinWinCode-Model", invocation.model_id);
                invocation.payload
            }
            HttpsSseProviderProtocol::AnthropicMessages(_) => {
                let prepared = anthropic_request
                    .as_ref()
                    .ok_or_else(ProviderAdapterError::protocol)?;
                request = request
                    .header("Content-Type", "application/json")
                    .header("Anthropic-Version", "2023-06-01")
                    .header("X-WinWinCode-Model", invocation.model_id);
                prepared.body.as_slice()
            }
            HttpsSseProviderProtocol::OpenAiChatGptResponses => {
                let account_id = authentication
                    .account_id
                    .as_deref()
                    .ok_or_else(ProviderAdapterError::rejected)?;
                request = request
                    .header("Content-Type", "application/json")
                    .header("ChatGPT-Account-ID", account_id)
                    .header("Originator", "winwincode");
                openai_request
                    .as_deref()
                    .ok_or_else(ProviderAdapterError::protocol)?
            }
        };
        let response = request.send(payload);
        drop(authentication);
        let Ok(response) = response else {
            self.abandon_retryable_open(invocation)?;
            return Err(ProviderAdapterError::unavailable());
        };
        let status = response.status().as_u16();
        if status == 429 {
            self.abandon_retryable_open(invocation)?;
            return Err(ProviderAdapterError::rate_limited());
        }
        if status >= 500 {
            self.abandon_retryable_open(invocation)?;
            return Err(ProviderAdapterError::unavailable());
        }
        if !(200..=299).contains(&status) {
            self.finish_open(invocation, StreamState::Fenced)?;
            return Err(ProviderAdapterError::rejected());
        }
        let content_type = response
            .headers()
            .get("content-type")
            .map(|value| value.to_str().unwrap_or(""));
        if !provider_event_stream_content_type(self.shared.config.protocol, content_type) {
            self.finish_open(invocation, StreamState::Fenced)?;
            return Err(ProviderAdapterError::protocol());
        }
        self.finish_open(invocation, StreamState::Open(Some(response.into_body())))?;
        ProviderAdapterOpenReceipt::try_new(invocation.adapter_request_id.to_owned())
    }
}

impl ProviderAdapterPort for HttpsSseProviderAdapter {
    fn provider_id(&self) -> &str {
        &self.shared.config.provider_id
    }

    fn open(
        &self,
        invocation: &ProviderAdapterInvocation<'_>,
        credential: &ResolvedSecret,
    ) -> Result<ProviderAdapterOpenReceipt, ProviderAdapterError> {
        self.open_https(
            HttpInvocation::from_adapter(invocation),
            credential.expose(),
        )
    }

    fn control(
        &self,
        model_exchange_id: &ModelExchangeId,
        adapter_request_id: &str,
        action: ProviderStreamControlAction,
    ) -> Result<(), ProviderAdapterError> {
        if !valid_token(adapter_request_id, MAX_ADAPTER_REQUEST_ID_BYTES) {
            return Err(ProviderAdapterError::protocol());
        }
        let mut streams = self
            .shared
            .streams
            .lock()
            .map_err(|_| ProviderAdapterError::unavailable())?;
        let record = streams
            .entry(adapter_request_id.to_owned())
            .or_insert_with(|| StreamRecord {
                model_exchange_id: model_exchange_id.clone(),
                invocation_digest: [0; 32],
                controls: 0,
                tool_bindings: AnthropicToolBindings::default(),
                state: StreamState::Fenced,
            });
        if record.model_exchange_id != *model_exchange_id {
            return Err(ProviderAdapterError::protocol());
        }
        let bit = control_bit(action);
        if record.controls & bit != 0 {
            return Ok(());
        }
        match action {
            ProviderStreamControlAction::Pause => {
                if matches!(record.state, StreamState::Open(_)) {
                    record.controls &= !CONTROL_RESUME;
                    record.controls |= CONTROL_PAUSE;
                }
            }
            ProviderStreamControlAction::Resume => {
                if matches!(record.state, StreamState::Open(_)) {
                    record.controls &= !CONTROL_PAUSE;
                    record.controls |= CONTROL_RESUME;
                }
            }
            ProviderStreamControlAction::Cancel => {
                record.state = StreamState::Cancelled;
                record.controls |= CONTROL_CANCEL;
            }
            ProviderStreamControlAction::Release => {
                record.state = StreamState::Released;
                record.controls |= CONTROL_RELEASE;
            }
        }
        Ok(())
    }
}

impl fmt::Debug for HttpsSseProviderAdapter {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("HttpsSseProviderAdapter")
            .field("provider_id", &self.shared.config.provider_id)
            .field("endpoint", &"[REDACTED]")
            .finish_non_exhaustive()
    }
}

struct ParsedStream {
    events: Vec<ProviderStreamEvent>,
    terminal: ProviderGatewayTerminal,
}

fn parse_sse(
    bytes: &[u8],
    max_event_bytes: usize,
    max_events: usize,
) -> Result<ParsedStream, HttpsSseProviderError> {
    if bytes.contains(&0) {
        return Err(HttpsSseProviderError::new(
            HttpsSseProviderErrorKind::Protocol,
        ));
    }
    let text = std::str::from_utf8(bytes)
        .map_err(|_| HttpsSseProviderError::new(HttpsSseProviderErrorKind::Protocol))?;
    let normalized = text.replace("\r\n", "\n");
    if normalized.contains('\r') {
        return Err(HttpsSseProviderError::new(
            HttpsSseProviderErrorKind::Protocol,
        ));
    }
    let mut wire_events = Vec::new();
    let mut data = String::new();
    for line in normalized.split('\n') {
        if line.len() > max_event_bytes {
            return Err(HttpsSseProviderError::new(
                HttpsSseProviderErrorKind::SizeLimit,
            ));
        }
        if line.is_empty() {
            dispatch_event(&mut wire_events, &mut data, max_event_bytes, max_events)?;
        } else if line.starts_with(':') || line == "event: message" {
        } else if let Some(value) = line.strip_prefix("data:") {
            if !data.is_empty() {
                data.push('\n');
            }
            data.push_str(value.strip_prefix(' ').unwrap_or(value));
            if data.len() > max_event_bytes {
                return Err(HttpsSseProviderError::new(
                    HttpsSseProviderErrorKind::SizeLimit,
                ));
            }
        } else {
            return Err(HttpsSseProviderError::new(
                HttpsSseProviderErrorKind::Protocol,
            ));
        }
    }
    dispatch_event(&mut wire_events, &mut data, max_event_bytes, max_events)?;
    canonical_events(wire_events)
}

fn parse_openai_responses_sse(
    bytes: &[u8],
    max_event_bytes: usize,
    max_events: usize,
) -> Result<ParsedStream, HttpsSseProviderError> {
    if bytes.contains(&0) {
        return Err(HttpsSseProviderError::new(
            HttpsSseProviderErrorKind::Protocol,
        ));
    }
    let text = std::str::from_utf8(bytes)
        .map_err(|_| HttpsSseProviderError::new(HttpsSseProviderErrorKind::Protocol))?;
    let normalized = text.replace("\r\n", "\n");
    if normalized.contains('\r') {
        return Err(HttpsSseProviderError::new(
            HttpsSseProviderErrorKind::Protocol,
        ));
    }
    let mut parser = OpenAiResponsesEvents::default();
    let mut data = String::new();
    let mut event_count = 0_usize;
    for line in normalized.split('\n') {
        if line.len() > max_event_bytes {
            return Err(HttpsSseProviderError::new(
                HttpsSseProviderErrorKind::SizeLimit,
            ));
        }
        if line.is_empty() {
            dispatch_openai_event(
                &mut parser,
                &mut data,
                &mut event_count,
                max_event_bytes,
                max_events,
            )?;
        } else if line.starts_with(':') || line.starts_with("event:") || line.starts_with("id:") {
        } else if let Some(value) = line.strip_prefix("data:") {
            if !data.is_empty() {
                data.push('\n');
            }
            data.push_str(value.strip_prefix(' ').unwrap_or(value));
            if data.len() > max_event_bytes {
                return Err(HttpsSseProviderError::new(
                    HttpsSseProviderErrorKind::SizeLimit,
                ));
            }
        } else {
            return Err(HttpsSseProviderError::new(
                HttpsSseProviderErrorKind::Protocol,
            ));
        }
    }
    dispatch_openai_event(
        &mut parser,
        &mut data,
        &mut event_count,
        max_event_bytes,
        max_events,
    )?;
    Ok(parser.finish())
}

fn dispatch_openai_event(
    parser: &mut OpenAiResponsesEvents,
    data: &mut String,
    event_count: &mut usize,
    max_event_bytes: usize,
    max_events: usize,
) -> Result<(), HttpsSseProviderError> {
    if data.is_empty() {
        return Ok(());
    }
    if data.len() > max_event_bytes || *event_count >= max_events {
        return Err(HttpsSseProviderError::new(
            HttpsSseProviderErrorKind::SizeLimit,
        ));
    }
    *event_count += 1;
    if data != "[DONE]" {
        let value: serde_json::Value = serde_json::from_str(data)
            .map_err(|_| HttpsSseProviderError::new(HttpsSseProviderErrorKind::Protocol))?;
        parser.push(&value)?;
    }
    data.clear();
    Ok(())
}

#[derive(Clone)]
enum OpenAiOutputKind {
    Text,
    Reasoning,
    Tool { provider_call_id: String },
}

#[derive(Clone)]
struct OpenAiOutputState {
    kind: OpenAiOutputKind,
    emitted_delta: bool,
    ended: bool,
}

#[derive(Default)]
struct OpenAiResponsesEvents {
    events: Vec<ProviderStreamEvent>,
    outputs: BTreeMap<u32, OpenAiOutputState>,
    response_started: bool,
    saw_tool: bool,
    terminal: Option<ProviderGatewayTerminal>,
}

impl OpenAiResponsesEvents {
    fn push(&mut self, value: &serde_json::Value) -> Result<(), HttpsSseProviderError> {
        if self.terminal.is_some() {
            return Err(HttpsSseProviderError::new(
                HttpsSseProviderErrorKind::Protocol,
            ));
        }
        let kind = value
            .get("type")
            .and_then(serde_json::Value::as_str)
            .ok_or_else(|| HttpsSseProviderError::new(HttpsSseProviderErrorKind::Protocol))?;
        match kind {
            "response.created" | "response.in_progress" => {
                self.ensure_response_started(response_id(value));
            }
            "response.output_item.added" => {
                self.ensure_response_started(response_id(value));
                let index = output_index(value)?;
                let item = value.get("item").ok_or_else(protocol_error)?;
                self.start_item(index, item)?;
            }
            "response.content_part.added" => {
                self.ensure_response_started(response_id(value));
                let index = output_index(value)?;
                self.ensure_text(index);
            }
            "response.output_text.delta" => {
                self.ensure_response_started(response_id(value));
                let index = output_index(value)?;
                self.ensure_text(index);
                let delta = required_string(value, "delta")?.to_owned();
                self.events
                    .push(ProviderStreamEvent::TextDelta { index, delta });
                if let Some(output) = self.outputs.get_mut(&index) {
                    output.emitted_delta = true;
                }
            }
            "response.output_text.done"
            | "response.function_call_arguments.done"
            | "response.custom_tool_call_input.done" => {
                self.end_output(output_index(value)?)?;
            }
            "response.reasoning_summary_text.delta" => {
                self.ensure_response_started(response_id(value));
                let index = output_index(value)?;
                self.ensure_reasoning(index);
                let summary_index = optional_u32(value, "summary_index").unwrap_or(0);
                self.events
                    .push(ProviderStreamEvent::ReasoningSummaryDelta {
                        index,
                        summary_index,
                        delta: required_string(value, "delta")?.to_owned(),
                    });
                if let Some(output) = self.outputs.get_mut(&index) {
                    output.emitted_delta = true;
                }
            }
            "response.reasoning_text.delta" => {
                self.ensure_response_started(response_id(value));
                let index = output_index(value)?;
                self.ensure_reasoning(index);
                self.events
                    .push(ProviderStreamEvent::ReasoningContentDelta {
                        index,
                        content_index: optional_u32(value, "content_index").unwrap_or(0),
                        delta: required_string(value, "delta")?.to_owned(),
                    });
                if let Some(output) = self.outputs.get_mut(&index) {
                    output.emitted_delta = true;
                }
            }
            "response.function_call_arguments.delta" | "response.custom_tool_call_input.delta" => {
                self.push_tool_delta(value)?;
            }
            "response.output_item.done" => {
                let index = output_index(value)?;
                let item = value.get("item").ok_or_else(protocol_error)?;
                self.finish_item(index, item)?;
            }
            "response.completed" => {
                self.complete(value, ProviderFinishReason::Stop)?;
            }
            "response.incomplete" => {
                self.complete(value, ProviderFinishReason::MaxTokens)?;
            }
            "response.failed" | "error" => {
                self.close_outputs()?;
                let failure = ProviderStreamFailure::new(ProviderStreamFailureKind::Server);
                self.events
                    .push(ProviderStreamEvent::Failed(failure.clone()));
                self.terminal = Some(ProviderGatewayTerminal::Failed {
                    failure: ModelAttemptFailureFact::from_stream(
                        failure.kind(),
                        ModelExecutionCertainty::AcceptanceUnknown,
                    ),
                    charge: None,
                });
            }
            _ => {}
        }
        Ok(())
    }

    fn ensure_response_started(&mut self, response_id: Option<&str>) {
        if !self.response_started {
            self.events.push(ProviderStreamEvent::ResponseStarted {
                provider_response_id: response_id.unwrap_or("openai-response").to_owned(),
            });
            self.response_started = true;
        }
    }

    fn start_item(
        &mut self,
        index: u32,
        item: &serde_json::Value,
    ) -> Result<(), HttpsSseProviderError> {
        if self.outputs.contains_key(&index) {
            return Ok(());
        }
        match item.get("type").and_then(serde_json::Value::as_str) {
            Some("message") => self.ensure_text(index),
            Some("reasoning") => self.ensure_reasoning(index),
            Some("function_call" | "custom_tool_call") => {
                let item_kind = item.get("type").and_then(serde_json::Value::as_str);
                let provider_call_id = item
                    .get("call_id")
                    .or_else(|| item.get("id"))
                    .and_then(serde_json::Value::as_str)
                    .ok_or_else(protocol_error)?
                    .to_owned();
                let name = required_string(item, "name")?.to_owned();
                let tool_kind = if item_kind == Some("custom_tool_call") {
                    ProviderToolKind::Custom
                } else {
                    ProviderToolKind::Function
                };
                let identity = ProviderToolIdentity::try_new(tool_kind, name, None)
                    .map_err(|_| protocol_error())?;
                self.events.push(ProviderStreamEvent::ToolCallStarted {
                    index,
                    provider_call_id: provider_call_id.clone(),
                    identity,
                });
                self.outputs.insert(
                    index,
                    OpenAiOutputState {
                        kind: OpenAiOutputKind::Tool { provider_call_id },
                        emitted_delta: false,
                        ended: false,
                    },
                );
                self.saw_tool = true;
            }
            _ => {}
        }
        Ok(())
    }

    fn ensure_text(&mut self, index: u32) {
        self.outputs.entry(index).or_insert_with(|| {
            self.events.push(ProviderStreamEvent::TextStarted { index });
            OpenAiOutputState {
                kind: OpenAiOutputKind::Text,
                emitted_delta: false,
                ended: false,
            }
        });
    }

    fn ensure_reasoning(&mut self, index: u32) {
        self.outputs.entry(index).or_insert_with(|| {
            self.events.push(ProviderStreamEvent::ReasoningStarted {
                index,
                summary_index: 0,
            });
            OpenAiOutputState {
                kind: OpenAiOutputKind::Reasoning,
                emitted_delta: false,
                ended: false,
            }
        });
    }

    fn push_tool_delta(&mut self, value: &serde_json::Value) -> Result<(), HttpsSseProviderError> {
        let index = output_index(value)?;
        let output = self.outputs.get_mut(&index).ok_or_else(protocol_error)?;
        let OpenAiOutputKind::Tool { provider_call_id } = &output.kind else {
            return Err(protocol_error());
        };
        self.events
            .push(ProviderStreamEvent::ToolCallArgumentsDelta {
                index,
                provider_call_id: provider_call_id.clone(),
                delta: required_string(value, "delta")?.to_owned(),
            });
        output.emitted_delta = true;
        Ok(())
    }

    fn finish_item(
        &mut self,
        index: u32,
        item: &serde_json::Value,
    ) -> Result<(), HttpsSseProviderError> {
        if !self.outputs.contains_key(&index) {
            self.start_item(index, item)?;
        }
        let needs_fallback = self
            .outputs
            .get(&index)
            .is_some_and(|output| !output.emitted_delta);
        if needs_fallback {
            match item.get("type").and_then(serde_json::Value::as_str) {
                Some("message") => {
                    let text = item
                        .get("content")
                        .and_then(serde_json::Value::as_array)
                        .into_iter()
                        .flatten()
                        .filter_map(|part| part.get("text").and_then(serde_json::Value::as_str))
                        .collect::<String>();
                    if !text.is_empty() {
                        self.events
                            .push(ProviderStreamEvent::TextDelta { index, delta: text });
                    }
                }
                Some("function_call") => {
                    if let Some(arguments) =
                        item.get("arguments").and_then(serde_json::Value::as_str)
                        && !arguments.is_empty()
                    {
                        self.push_tool_fallback(index, arguments.to_owned())?;
                    }
                }
                Some("custom_tool_call") => {
                    if let Some(input) = item.get("input").and_then(serde_json::Value::as_str)
                        && !input.is_empty()
                    {
                        self.push_tool_fallback(index, input.to_owned())?;
                    }
                }
                _ => {}
            }
        }
        self.end_output(index)
    }

    fn push_tool_fallback(
        &mut self,
        index: u32,
        delta: String,
    ) -> Result<(), HttpsSseProviderError> {
        let output = self.outputs.get(&index).ok_or_else(protocol_error)?;
        let OpenAiOutputKind::Tool { provider_call_id } = &output.kind else {
            return Err(protocol_error());
        };
        self.events
            .push(ProviderStreamEvent::ToolCallArgumentsDelta {
                index,
                provider_call_id: provider_call_id.clone(),
                delta,
            });
        Ok(())
    }

    fn end_output(&mut self, index: u32) -> Result<(), HttpsSseProviderError> {
        let output = self.outputs.get_mut(&index).ok_or_else(protocol_error)?;
        if output.ended {
            return Ok(());
        }
        match &output.kind {
            OpenAiOutputKind::Text => self.events.push(ProviderStreamEvent::TextEnded { index }),
            OpenAiOutputKind::Reasoning => {
                self.events
                    .push(ProviderStreamEvent::ReasoningEnded { index });
            }
            OpenAiOutputKind::Tool { provider_call_id } => {
                self.events.push(ProviderStreamEvent::ToolCallEnded {
                    index,
                    provider_call_id: provider_call_id.clone(),
                });
            }
        }
        output.ended = true;
        Ok(())
    }

    fn close_outputs(&mut self) -> Result<(), HttpsSseProviderError> {
        let indexes = self.outputs.keys().copied().collect::<Vec<_>>();
        for index in indexes {
            self.end_output(index)?;
        }
        Ok(())
    }

    fn complete(
        &mut self,
        value: &serde_json::Value,
        default_reason: ProviderFinishReason,
    ) -> Result<(), HttpsSseProviderError> {
        self.ensure_response_started(response_id(value));
        self.close_outputs()?;
        let response = value.get("response").unwrap_or(value);
        let usage = response.get("usage").unwrap_or(&serde_json::Value::Null);
        let token_usage = ProviderTokenUsage {
            input_tokens: json_u64(usage, "input_tokens"),
            cached_input_tokens: usage
                .pointer("/input_tokens_details/cached_tokens")
                .and_then(serde_json::Value::as_u64)
                .unwrap_or(0),
            cache_write_input_tokens: 0,
            output_tokens: json_u64(usage, "output_tokens"),
            reasoning_output_tokens: usage
                .pointer("/output_tokens_details/reasoning_tokens")
                .and_then(serde_json::Value::as_u64)
                .unwrap_or(0),
        };
        self.events.push(ProviderStreamEvent::Usage(token_usage));
        let reason = if self.saw_tool {
            ProviderFinishReason::ToolCalls
        } else {
            default_reason
        };
        self.events.push(ProviderStreamEvent::Finished(reason));
        self.terminal = Some(ProviderGatewayTerminal::Completed {
            usage: token_usage,
            actual_cost_micros: 0,
        });
        Ok(())
    }

    fn finish(mut self) -> ParsedStream {
        if self.terminal.is_none() {
            self.events.push(ProviderStreamEvent::Disconnected);
            self.terminal = Some(ProviderGatewayTerminal::Failed {
                failure: ModelAttemptFailureFact::from_stream(
                    ProviderStreamFailureKind::Transport,
                    ModelExecutionCertainty::AcceptanceUnknown,
                ),
                charge: None,
            });
        }
        ParsedStream {
            events: self.events,
            terminal: self.terminal.expect("terminal was synthesized"),
        }
    }
}

fn response_id(value: &serde_json::Value) -> Option<&str> {
    value
        .pointer("/response/id")
        .or_else(|| value.get("response_id"))
        .and_then(serde_json::Value::as_str)
}

fn output_index(value: &serde_json::Value) -> Result<u32, HttpsSseProviderError> {
    optional_u32(value, "output_index").ok_or_else(protocol_error)
}

fn optional_u32(value: &serde_json::Value, name: &str) -> Option<u32> {
    value
        .get(name)
        .and_then(serde_json::Value::as_u64)
        .and_then(|value| u32::try_from(value).ok())
}

fn required_string<'a>(
    value: &'a serde_json::Value,
    name: &str,
) -> Result<&'a str, HttpsSseProviderError> {
    value
        .get(name)
        .and_then(serde_json::Value::as_str)
        .ok_or_else(protocol_error)
}

fn json_u64(value: &serde_json::Value, name: &str) -> u64 {
    value
        .get(name)
        .and_then(serde_json::Value::as_u64)
        .unwrap_or(0)
}

fn protocol_error() -> HttpsSseProviderError {
    HttpsSseProviderError::new(HttpsSseProviderErrorKind::Protocol)
}

fn dispatch_event(
    events: &mut Vec<ProviderWireEvent>,
    data: &mut String,
    max_event_bytes: usize,
    max_events: usize,
) -> Result<(), HttpsSseProviderError> {
    if data.is_empty() {
        return Ok(());
    }
    if data.len() > max_event_bytes || events.len() >= max_events {
        return Err(HttpsSseProviderError::new(
            HttpsSseProviderErrorKind::SizeLimit,
        ));
    }
    let event = serde_json::from_str(data)
        .map_err(|_| HttpsSseProviderError::new(HttpsSseProviderErrorKind::Protocol))?;
    events.push(event);
    data.clear();
    Ok(())
}

fn canonical_events(wire: Vec<ProviderWireEvent>) -> Result<ParsedStream, HttpsSseProviderError> {
    let mut canonical = CanonicalEvents::new(wire.len());
    for event in wire {
        if canonical.terminal.is_some() {
            return Err(HttpsSseProviderError::new(
                HttpsSseProviderErrorKind::Protocol,
            ));
        }
        canonical.push(event)?;
    }
    Ok(canonical.finish())
}

struct CanonicalEvents {
    events: Vec<ProviderStreamEvent>,
    usage: Option<ProviderTokenUsage>,
    cost_micros: Option<u64>,
    terminal: Option<ProviderGatewayTerminal>,
}

impl CanonicalEvents {
    fn new(capacity: usize) -> Self {
        Self {
            events: Vec::with_capacity(capacity.saturating_add(1)),
            usage: None,
            cost_micros: None,
            terminal: None,
        }
    }

    fn push(&mut self, event: ProviderWireEvent) -> Result<(), HttpsSseProviderError> {
        if event.is_accounting_or_terminal() {
            self.push_accounting_or_terminal(event)
        } else {
            self.push_output(event)
        }
    }

    fn push_output(&mut self, event: ProviderWireEvent) -> Result<(), HttpsSseProviderError> {
        let event = match event {
            ProviderWireEvent::ResponseStarted { response_id } => {
                ProviderStreamEvent::ResponseStarted {
                    provider_response_id: response_id,
                }
            }
            ProviderWireEvent::TextStarted { index } => ProviderStreamEvent::TextStarted { index },
            ProviderWireEvent::TextDelta { index, delta } => {
                ProviderStreamEvent::TextDelta { index, delta }
            }
            ProviderWireEvent::TextEnded { index } => ProviderStreamEvent::TextEnded { index },
            ProviderWireEvent::ReasoningStarted {
                index,
                summary_index,
            } => ProviderStreamEvent::ReasoningStarted {
                index,
                summary_index,
            },
            ProviderWireEvent::ReasoningSummaryDelta {
                index,
                summary_index,
                delta,
            } => ProviderStreamEvent::ReasoningSummaryDelta {
                index,
                summary_index,
                delta,
            },
            ProviderWireEvent::ReasoningContentDelta {
                index,
                content_index,
                delta,
            } => ProviderStreamEvent::ReasoningContentDelta {
                index,
                content_index,
                delta,
            },
            ProviderWireEvent::ReasoningEnded { index } => {
                ProviderStreamEvent::ReasoningEnded { index }
            }
            ProviderWireEvent::ToolCallStarted {
                index,
                provider_call_id,
                name,
                namespace,
                kind,
            } => ProviderStreamEvent::ToolCallStarted {
                index,
                provider_call_id,
                identity: ProviderToolIdentity::try_new(kind.into_tool_kind(), name, namespace)
                    .map_err(|_| HttpsSseProviderError::new(HttpsSseProviderErrorKind::Protocol))?,
            },
            ProviderWireEvent::ToolCallArgumentsDelta {
                index,
                provider_call_id,
                delta,
            } => ProviderStreamEvent::ToolCallArgumentsDelta {
                index,
                provider_call_id,
                delta,
            },
            ProviderWireEvent::ToolCallEnded {
                index,
                provider_call_id,
            } => ProviderStreamEvent::ToolCallEnded {
                index,
                provider_call_id,
            },
            _ => {
                return Err(HttpsSseProviderError::new(
                    HttpsSseProviderErrorKind::Protocol,
                ));
            }
        };
        self.events.push(event);
        Ok(())
    }

    fn push_accounting_or_terminal(
        &mut self,
        event: ProviderWireEvent,
    ) -> Result<(), HttpsSseProviderError> {
        match event {
            ProviderWireEvent::Usage {
                input_tokens,
                cached_input_tokens,
                cache_write_input_tokens,
                output_tokens,
                reasoning_output_tokens,
                actual_cost_micros,
            } => {
                let value = ProviderTokenUsage {
                    input_tokens,
                    cached_input_tokens,
                    cache_write_input_tokens,
                    output_tokens,
                    reasoning_output_tokens,
                };
                if self.usage.replace(value).is_some()
                    || self.cost_micros.replace(actual_cost_micros).is_some()
                    || actual_cost_micros > MAX_SAFE_INTEGER
                {
                    return Err(HttpsSseProviderError::new(
                        HttpsSseProviderErrorKind::Protocol,
                    ));
                }
                self.events.push(ProviderStreamEvent::Usage(value));
            }
            ProviderWireEvent::Finished { reason } => {
                let usage = self.usage.ok_or_else(|| {
                    HttpsSseProviderError::new(HttpsSseProviderErrorKind::Protocol)
                })?;
                let cost = self.cost_micros.ok_or_else(|| {
                    HttpsSseProviderError::new(HttpsSseProviderErrorKind::Protocol)
                })?;
                let reason = reason.into_finish_reason();
                self.events.push(ProviderStreamEvent::Finished(reason));
                self.terminal = Some(ProviderGatewayTerminal::Completed {
                    usage,
                    actual_cost_micros: cost,
                });
            }
            ProviderWireEvent::Failed { kind, status } => {
                let failure = status.map_or_else(
                    || ProviderStreamFailure::new(kind.into_failure_kind()),
                    |status| {
                        ProviderStreamFailure::new(kind.into_failure_kind()).with_status(status)
                    },
                );
                self.events
                    .push(ProviderStreamEvent::Failed(failure.clone()));
                self.terminal = Some(ProviderGatewayTerminal::Failed {
                    failure: ModelAttemptFailureFact::from_stream(
                        failure.kind(),
                        ModelExecutionCertainty::AcceptanceUnknown,
                    ),
                    charge: None,
                });
            }
            ProviderWireEvent::Cancelled => {
                self.events.push(ProviderStreamEvent::Cancelled);
                self.terminal = Some(ProviderGatewayTerminal::Cancelled);
            }
            ProviderWireEvent::Disconnected => {
                self.events.push(ProviderStreamEvent::Disconnected);
                self.terminal = Some(ProviderGatewayTerminal::Failed {
                    failure: ModelAttemptFailureFact::from_stream(
                        ProviderStreamFailureKind::Transport,
                        ModelExecutionCertainty::AcceptanceUnknown,
                    ),
                    charge: None,
                });
            }
            _ => {
                return Err(HttpsSseProviderError::new(
                    HttpsSseProviderErrorKind::Protocol,
                ));
            }
        }
        Ok(())
    }

    fn finish(mut self) -> ParsedStream {
        if self.terminal.is_none() {
            self.events.push(ProviderStreamEvent::Disconnected);
            self.terminal = Some(ProviderGatewayTerminal::Failed {
                failure: ModelAttemptFailureFact::from_stream(
                    ProviderStreamFailureKind::Transport,
                    ModelExecutionCertainty::AcceptanceUnknown,
                ),
                charge: None,
            });
        }
        ParsedStream {
            events: self.events,
            terminal: self.terminal.expect("terminal was synthesized"),
        }
    }
}

#[derive(Deserialize)]
#[serde(
    tag = "type",
    rename_all = "camelCase",
    rename_all_fields = "camelCase",
    deny_unknown_fields
)]
enum ProviderWireEvent {
    #[serde(rename = "response.started")]
    ResponseStarted { response_id: String },
    #[serde(rename = "text.started")]
    TextStarted { index: u32 },
    #[serde(rename = "text.delta")]
    TextDelta { index: u32, delta: String },
    #[serde(rename = "text.ended")]
    TextEnded { index: u32 },
    #[serde(rename = "reasoning.started")]
    ReasoningStarted { index: u32, summary_index: u32 },
    #[serde(rename = "reasoning.summary_delta")]
    ReasoningSummaryDelta {
        index: u32,
        summary_index: u32,
        delta: String,
    },
    #[serde(rename = "reasoning.content_delta")]
    ReasoningContentDelta {
        index: u32,
        content_index: u32,
        delta: String,
    },
    #[serde(rename = "reasoning.ended")]
    ReasoningEnded { index: u32 },
    #[serde(rename = "tool_call.started")]
    ToolCallStarted {
        index: u32,
        provider_call_id: String,
        name: String,
        namespace: Option<String>,
        kind: WireToolKind,
    },
    #[serde(rename = "tool_call.arguments_delta")]
    ToolCallArgumentsDelta {
        index: u32,
        provider_call_id: String,
        delta: String,
    },
    #[serde(rename = "tool_call.ended")]
    ToolCallEnded {
        index: u32,
        provider_call_id: String,
    },
    #[serde(rename = "usage")]
    Usage {
        input_tokens: u64,
        cached_input_tokens: u64,
        cache_write_input_tokens: u64,
        output_tokens: u64,
        reasoning_output_tokens: u64,
        actual_cost_micros: u64,
    },
    #[serde(rename = "response.finished")]
    Finished { reason: WireFinishReason },
    #[serde(rename = "response.failed")]
    Failed {
        kind: WireFailureKind,
        status: Option<u16>,
    },
    #[serde(rename = "response.cancelled")]
    Cancelled,
    #[serde(rename = "response.disconnected")]
    Disconnected,
}

impl ProviderWireEvent {
    const fn is_accounting_or_terminal(&self) -> bool {
        matches!(
            self,
            Self::Usage { .. }
                | Self::Finished { .. }
                | Self::Failed { .. }
                | Self::Cancelled
                | Self::Disconnected
        )
    }
}

#[derive(Deserialize)]
#[serde(rename_all = "snake_case")]
enum WireFinishReason {
    Stop,
    ToolCalls,
    MaxTokens,
}

#[derive(Deserialize)]
#[serde(rename_all = "snake_case")]
enum WireToolKind {
    Function,
    Custom,
}

impl WireToolKind {
    const fn into_tool_kind(self) -> ProviderToolKind {
        match self {
            Self::Function => ProviderToolKind::Function,
            Self::Custom => ProviderToolKind::Custom,
        }
    }
}

impl WireFinishReason {
    const fn into_finish_reason(self) -> ProviderFinishReason {
        match self {
            Self::Stop => ProviderFinishReason::Stop,
            Self::ToolCalls => ProviderFinishReason::ToolCalls,
            Self::MaxTokens => ProviderFinishReason::MaxTokens,
        }
    }
}

#[derive(Clone, Copy, Deserialize)]
#[serde(rename_all = "snake_case")]
enum WireFailureKind {
    Authentication,
    InvalidRequest,
    RateLimit,
    Quota,
    Timeout,
    Transport,
    Server,
    ContextWindowExceeded,
    Unknown,
}

impl WireFailureKind {
    const fn into_failure_kind(self) -> ProviderStreamFailureKind {
        match self {
            Self::Authentication => ProviderStreamFailureKind::Authentication,
            Self::InvalidRequest => ProviderStreamFailureKind::InvalidRequest,
            Self::RateLimit => ProviderStreamFailureKind::RateLimit,
            Self::Quota => ProviderStreamFailureKind::Quota,
            Self::Timeout => ProviderStreamFailureKind::Timeout,
            Self::Transport => ProviderStreamFailureKind::Transport,
            Self::Server => ProviderStreamFailureKind::Server,
            Self::ContextWindowExceeded => ProviderStreamFailureKind::ContextWindowExceeded,
            Self::Unknown => ProviderStreamFailureKind::Unknown,
        }
    }
}

fn map_anthropic_configuration(
    _error: crate::provider_anthropic::AnthropicCodecError,
) -> HttpsSseProviderError {
    HttpsSseProviderError::new(HttpsSseProviderErrorKind::InvalidConfiguration)
}

fn map_anthropic_request(
    error: crate::provider_anthropic::AnthropicCodecError,
) -> ProviderAdapterError {
    match error.kind() {
        AnthropicCodecErrorKind::InvalidRequest | AnthropicCodecErrorKind::SizeLimit => {
            ProviderAdapterError::rejected()
        }
        AnthropicCodecErrorKind::Protocol => ProviderAdapterError::protocol(),
    }
}

fn map_anthropic_response(
    error: crate::provider_anthropic::AnthropicCodecError,
) -> HttpsSseProviderError {
    HttpsSseProviderError::new(match error.kind() {
        AnthropicCodecErrorKind::InvalidRequest | AnthropicCodecErrorKind::Protocol => {
            HttpsSseProviderErrorKind::Protocol
        }
        AnthropicCodecErrorKind::SizeLimit => HttpsSseProviderErrorKind::SizeLimit,
    })
}

fn invocation_digest(invocation: HttpInvocation<'_>) -> [u8; 32] {
    let mut digest = Sha256::new();
    digest.update(b"winwincode.https-sse-provider-invocation.v1\0");
    for value in [
        invocation.model_exchange_id.0.as_bytes(),
        invocation.request_id.0.as_bytes(),
        invocation.adapter_request_id.as_bytes(),
        invocation.model_id.as_bytes(),
        invocation.content_type.as_bytes(),
        invocation.payload,
    ] {
        digest.update(u64::try_from(value.len()).unwrap_or(u64::MAX).to_be_bytes());
        digest.update(value);
    }
    digest.finalize().into()
}

fn authorization_value(secret: &[u8]) -> Result<String, ProviderAdapterError> {
    if secret.is_empty()
        || secret.len() > 16 * 1024
        || !secret.iter().all(|byte| (0x21..=0x7e).contains(byte))
    {
        return Err(ProviderAdapterError::rejected());
    }
    let mut value = Vec::with_capacity(AUTHORIZATION_PREFIX.len() + secret.len());
    value.extend_from_slice(AUTHORIZATION_PREFIX);
    value.extend_from_slice(secret);
    String::from_utf8(value).map_err(|_| ProviderAdapterError::rejected())
}

struct OpenAiRequestAuthentication {
    authorization: String,
    account_id: Option<String>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct OpenAiStoredCredential<'credential> {
    kind: &'credential str,
    schema: &'credential str,
    access_token: &'credential str,
    account_id: &'credential str,
}

impl Drop for OpenAiRequestAuthentication {
    fn drop(&mut self) {
        let mut authorization = std::mem::take(&mut self.authorization).into_bytes();
        authorization.fill(0);
        if let Some(account_id) = self.account_id.take() {
            let mut account_id = account_id.into_bytes();
            account_id.fill(0);
        }
    }
}

fn openai_authentication(
    secret: &[u8],
) -> Result<OpenAiRequestAuthentication, ProviderAdapterError> {
    if secret.is_empty() || secret.len() > 256 * 1024 {
        return Err(ProviderAdapterError::rejected());
    }
    let credential: OpenAiStoredCredential<'_> =
        serde_json::from_slice(secret).map_err(|_| ProviderAdapterError::rejected())?;
    if credential.kind != "credential" {
        return Err(ProviderAdapterError::rejected());
    }
    if credential.schema != "winwincode.provider-credential.v1" {
        return Err(ProviderAdapterError::rejected());
    }
    if !valid_header_value(credential.account_id, 512) {
        return Err(ProviderAdapterError::rejected());
    }
    Ok(OpenAiRequestAuthentication {
        authorization: authorization_value(credential.access_token.as_bytes())?,
        account_id: Some(credential.account_id.to_owned()),
    })
}

fn prepare_openai_responses_request(
    payload: &[u8],
    model_id: &str,
) -> Result<Vec<u8>, ProviderAdapterError> {
    if payload.is_empty() || payload.len() > 16 * 1024 * 1024 {
        return Err(ProviderAdapterError::rejected());
    }
    let mut value: serde_json::Value =
        serde_json::from_slice(payload).map_err(|_| ProviderAdapterError::rejected())?;
    let object = value
        .as_object_mut()
        .ok_or_else(ProviderAdapterError::rejected)?;
    object.insert(
        "model".to_owned(),
        serde_json::Value::String(model_id.to_owned()),
    );
    object.insert("stream".to_owned(), serde_json::Value::Bool(true));
    serde_json::to_vec(&value).map_err(|_| ProviderAdapterError::protocol())
}

fn valid_header_value(value: &str, max_bytes: usize) -> bool {
    !value.is_empty()
        && value.len() <= max_bytes
        && value
            .bytes()
            .all(|byte| byte == b'\t' || (0x20..=0x7e).contains(&byte))
}

fn canonical_https_endpoint(value: &str) -> bool {
    if value.trim() != value {
        return false;
    }
    let Ok(uri) = ureq::http::Uri::from_str(value) else {
        return false;
    };
    uri.scheme_str() == Some("https")
        && uri.authority().is_some_and(|authority| {
            !authority.as_str().contains('@') && !authority.host().is_empty()
        })
        && uri
            .path_and_query()
            .is_none_or(|path| path.query().is_none())
}

fn canonical_event_stream_content_type(value: &str) -> bool {
    let normalized = value.to_ascii_lowercase();
    matches!(
        normalized.as_str(),
        "text/event-stream" | "text/event-stream; charset=utf-8"
    )
}

fn provider_event_stream_content_type(
    protocol: HttpsSseProviderProtocol,
    value: Option<&str>,
) -> bool {
    match value {
        Some(value) => canonical_event_stream_content_type(value),
        None => matches!(protocol, HttpsSseProviderProtocol::OpenAiChatGptResponses),
    }
}

const fn control_bit(action: ProviderStreamControlAction) -> u8 {
    match action {
        ProviderStreamControlAction::Pause => CONTROL_PAUSE,
        ProviderStreamControlAction::Resume => CONTROL_RESUME,
        ProviderStreamControlAction::Cancel => CONTROL_CANCEL,
        ProviderStreamControlAction::Release => CONTROL_RELEASE,
    }
}

fn valid_token(value: &str, max_len: usize) -> bool {
    !value.is_empty()
        && value.len() <= max_len
        && value.trim() == value
        && !value.chars().any(char::is_control)
}

#[cfg(test)]
mod tests {
    use std::{
        io::{Read, Write},
        net::{TcpListener, TcpStream},
        sync::{Arc, mpsc},
        thread,
    };

    use rcgen::{CertifiedKey, generate_simple_self_signed};
    use rustls::{
        ServerConfig, ServerConnection, StreamOwned,
        pki_types::{PrivateKeyDer, PrivatePkcs8KeyDer},
    };

    use super::*;

    #[test]
    fn form_encoding_does_not_leave_oauth_values_unescaped() {
        assert_eq!(form_component("a b+c/=?"), "a%20b%2Bc%2F%3D%3F");
    }

    #[test]
    fn device_authorization_configuration_is_pinned_to_openai_https() {
        let config = OpenAiDeviceAuthorizationConfig::production();
        assert!(OpenAiDeviceAuthorizationAdapter::try_new(config).is_ok());
    }

    const SECRET: &[u8] = b"provider-https-sse-secret-fixture";
    const PAYLOAD: &[u8] = br#"{"input":"local TLS fixture"}"#;

    struct TestResponse {
        status: &'static str,
        content_type: &'static str,
        body: &'static str,
        declared_length: Option<usize>,
        delay: Duration,
    }

    struct TlsFixture {
        endpoint: String,
        certificate_der: Vec<u8>,
        requests: mpsc::Receiver<Vec<u8>>,
        server: thread::JoinHandle<()>,
    }

    impl TlsFixture {
        fn start(responses: Vec<TestResponse>) -> Self {
            let _ = rustls::crypto::aws_lc_rs::default_provider().install_default();
            let CertifiedKey { cert, signing_key } =
                generate_simple_self_signed(vec!["localhost".to_owned()])
                    .expect("generate TLS fixture certificate");
            let private_key =
                PrivateKeyDer::Pkcs8(PrivatePkcs8KeyDer::from(signing_key.serialize_der()));
            let config = ServerConfig::builder()
                .with_no_client_auth()
                .with_single_cert(vec![cert.der().clone()], private_key)
                .expect("build TLS fixture server config");
            let listener = TcpListener::bind("127.0.0.1:0").expect("bind TLS fixture");
            let address = listener.local_addr().expect("TLS fixture address");
            let (request_tx, requests) = mpsc::channel();
            let server = thread::spawn(move || {
                let config = Arc::new(config);
                for response in responses {
                    let (socket, _) = listener.accept().expect("accept TLS request");
                    let connection =
                        ServerConnection::new(Arc::clone(&config)).expect("TLS connection");
                    let mut stream = StreamOwned::new(connection, socket);
                    let request = read_http_request(&mut stream);
                    request_tx.send(request).expect("record TLS request");
                    thread::sleep(response.delay);
                    write_http_response(&mut stream, &response);
                }
            });
            Self {
                endpoint: format!("https://localhost:{}/v1/model", address.port()),
                certificate_der: cert.der().to_vec(),
                requests,
                server,
            }
        }

        fn finish(self) -> Vec<Vec<u8>> {
            self.server.join().expect("join TLS fixture");
            self.requests.try_iter().collect()
        }
    }

    fn read_http_request(stream: &mut StreamOwned<ServerConnection, TcpStream>) -> Vec<u8> {
        let mut request = Vec::new();
        let mut buffer = [0_u8; 4 * 1024];
        loop {
            let count = stream.read(&mut buffer).expect("read TLS request");
            assert_ne!(count, 0, "request closed before the declared body");
            request.extend_from_slice(&buffer[..count]);
            let Some(header_end) = find_bytes(&request, b"\r\n\r\n") else {
                continue;
            };
            let content_length = content_length(&request[..header_end]);
            if request.len() >= header_end + 4 + content_length {
                return request;
            }
        }
    }

    fn write_http_response(
        stream: &mut StreamOwned<ServerConnection, TcpStream>,
        response: &TestResponse,
    ) {
        write!(
            stream,
            "HTTP/1.1 {}\r\nContent-Type: {}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
            response.status,
            response.content_type,
            response.declared_length.unwrap_or(response.body.len()),
            response.body
        )
        .expect("write TLS response");
        stream.flush().expect("flush TLS response");
    }

    fn find_bytes(haystack: &[u8], needle: &[u8]) -> Option<usize> {
        haystack
            .windows(needle.len())
            .position(|window| window == needle)
    }

    fn contains_ascii_case_insensitive(haystack: &[u8], needle: &[u8]) -> bool {
        haystack.windows(needle.len()).any(|window| {
            window
                .iter()
                .zip(needle)
                .all(|(left, right)| left.eq_ignore_ascii_case(right))
        })
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

    fn config(fixture: &TlsFixture) -> HttpsSseProviderConfig {
        HttpsSseProviderConfig::try_new(
            "provider-https-fixture".to_owned(),
            fixture.endpoint.clone(),
            HttpsSseProviderTimeouts {
                connect: Duration::from_secs(2),
                first_byte: Duration::from_secs(2),
                idle: Duration::from_secs(2),
                total: Duration::from_secs(5),
            },
            HttpsSseProviderLimits {
                response_bytes: 64 * 1024,
                event_bytes: 8 * 1024,
                events: 64,
            },
        )
        .expect("HTTPS/SSE fixture config")
        .with_specific_tls_roots(vec![fixture.certificate_der.clone()])
        .expect("fixture TLS root")
    }

    fn invocation<'a>(
        exchange: &'a ModelExchangeId,
        request_id: &'a RequestId,
    ) -> HttpInvocation<'a> {
        HttpInvocation {
            model_exchange_id: exchange,
            request_id,
            adapter_request_id: "pad_00000000000000000000000001",
            model_id: "fixture-model",
            content_type: "application/json",
            payload: PAYLOAD,
        }
    }

    fn successful_sse() -> &'static str {
        concat!(
            "data: {\"type\":\"response.started\",\"responseId\":\"response-1\"}\n\n",
            "data: {\"type\":\"text.started\",\"index\":0}\n\n",
            "data: {\"type\":\"text.delta\",\"index\":0,\"delta\":\"hello\"}\n\n",
            "data: {\"type\":\"text.ended\",\"index\":0}\n\n",
            "data: {\"type\":\"usage\",\"inputTokens\":11,\"cachedInputTokens\":2,",
            "\"cacheWriteInputTokens\":3,\"outputTokens\":5,\"reasoningOutputTokens\":7,",
            "\"actualCostMicros\":19}\n\n",
            "data: {\"type\":\"response.finished\",\"reason\":\"stop\"}\n\n"
        )
    }

    fn parsing_error_kind(
        result: Result<ParsedStream, HttpsSseProviderError>,
    ) -> HttpsSseProviderErrorKind {
        match result {
            Ok(_) => panic!("expected SSE parsing failure"),
            Err(error) => error.kind(),
        }
    }

    #[test]
    fn verified_tls_open_uses_bounded_headers_and_exact_idempotency() {
        let fixture = TlsFixture::start(vec![TestResponse {
            status: "200 OK",
            content_type: "text/event-stream",
            body: successful_sse(),
            declared_length: None,
            delay: Duration::ZERO,
        }]);
        let adapter = HttpsSseProviderAdapter::try_new(config(&fixture)).expect("HTTPS adapter");
        let exchange = ModelExchangeId("mdl_00000000000000000000000001".to_owned());
        let request_id = RequestId("req_00000000000000000000000001".to_owned());
        let request = invocation(&exchange, &request_id);
        let first = adapter
            .open_https(request, SECRET)
            .expect("open verified TLS stream");
        let replay = adapter
            .open_https(request, SECRET)
            .expect("replay exact Provider open");
        assert_eq!(first, replay);
        assert!(!format!("{adapter:?}").contains("provider-https-sse-secret"));

        let requests = fixture.finish();
        assert_eq!(
            requests.len(),
            1,
            "exact replay must not make a second call"
        );
        let request = &requests[0];
        assert!(request.windows(SECRET.len()).any(|window| window == SECRET));
        assert!(
            request
                .windows(PAYLOAD.len())
                .any(|window| window == PAYLOAD)
        );
        assert!(contains_ascii_case_insensitive(
            request,
            b"Idempotency-Key: pad_00000"
        ));
    }

    #[test]
    fn cancellation_while_tls_open_is_pending_fences_the_late_response() {
        let fixture = TlsFixture::start(vec![TestResponse {
            status: "200 OK",
            content_type: "text/event-stream",
            body: successful_sse(),
            declared_length: None,
            delay: Duration::from_millis(100),
        }]);
        let adapter = HttpsSseProviderAdapter::try_new(config(&fixture)).expect("HTTPS adapter");
        let exchange = ModelExchangeId("mdl_00000000000000000000000004".to_owned());
        let request_id = RequestId("req_00000000000000000000000004".to_owned());
        let opener_adapter = adapter.clone();
        let opener_exchange = exchange.clone();
        let opener_request_id = request_id.clone();
        let opener = thread::spawn(move || {
            opener_adapter.open_https(invocation(&opener_exchange, &opener_request_id), SECRET)
        });
        fixture
            .requests
            .recv_timeout(Duration::from_secs(2))
            .expect("pending TLS request");
        adapter
            .control(
                &exchange,
                "pad_00000000000000000000000001",
                ProviderStreamControlAction::Cancel,
            )
            .expect("cancel pending Provider open");
        assert_eq!(
            opener.join().expect("join pending Provider open"),
            Err(ProviderAdapterError::rejected())
        );
        assert_eq!(
            adapter.open_https(invocation(&exchange, &request_id), SECRET),
            Err(ProviderAdapterError::rejected())
        );
        assert!(fixture.finish().is_empty());
    }

    #[test]
    fn retryable_status_reuses_identity_and_sse_is_strict_and_accounted() {
        let fixture = TlsFixture::start(vec![
            TestResponse {
                status: "500 Internal Server Error",
                content_type: "application/json",
                body: "{}",
                declared_length: None,
                delay: Duration::ZERO,
            },
            TestResponse {
                status: "429 Too Many Requests",
                content_type: "application/json",
                body: "{}",
                declared_length: None,
                delay: Duration::ZERO,
            },
            TestResponse {
                status: "200 OK",
                content_type: "text/event-stream; charset=utf-8",
                body: successful_sse(),
                declared_length: None,
                delay: Duration::ZERO,
            },
        ]);
        let adapter = HttpsSseProviderAdapter::try_new(config(&fixture)).expect("HTTPS adapter");
        let exchange = ModelExchangeId("mdl_00000000000000000000000002".to_owned());
        let request_id = RequestId("req_00000000000000000000000002".to_owned());
        let request = invocation(&exchange, &request_id);
        assert_eq!(
            adapter.open_https(request, SECRET).expect_err("5xx"),
            ProviderAdapterError::unavailable()
        );
        assert_eq!(
            adapter.open_https(request, SECRET).expect_err("429"),
            ProviderAdapterError::rate_limited()
        );
        adapter
            .open_https(request, SECRET)
            .expect("retry exact idempotency identity");
        let requests = fixture.finish();
        assert_eq!(requests.len(), 3);
        assert!(
            requests
                .iter()
                .all(|request| contains_ascii_case_insensitive(
                    request,
                    b"Idempotency-Key: pad_00000"
                ))
        );

        let parsed =
            parse_sse(successful_sse().as_bytes(), 8 * 1024, 64).expect("parse strict SSE");
        assert_eq!(parsed.events.len(), 6);
        assert!(matches!(
            parsed.terminal,
            ProviderGatewayTerminal::Completed {
                usage: ProviderTokenUsage {
                    input_tokens: 11,
                    cached_input_tokens: 2,
                    cache_write_input_tokens: 3,
                    output_tokens: 5,
                    reasoning_output_tokens: 7,
                },
                actual_cost_micros: 19,
            }
        ));
        assert_eq!(
            parsing_error_kind(parse_sse(b"data: {\"type\":\"unknown\"}\n\n", 1024, 2,)),
            HttpsSseProviderErrorKind::Protocol
        );
        assert_eq!(
            parsing_error_kind(parse_sse(b"data: {}\n\n", 4, 2)),
            HttpsSseProviderErrorKind::SizeLimit
        );
    }

    #[test]
    fn cancellation_and_release_fence_late_or_foreign_open() {
        let fixture = TlsFixture::start(Vec::new());
        let adapter = HttpsSseProviderAdapter::try_new(config(&fixture)).expect("HTTPS adapter");
        let exchange = ModelExchangeId("mdl_00000000000000000000000003".to_owned());
        let request_id = RequestId("req_00000000000000000000000003".to_owned());
        let request = invocation(&exchange, &request_id);
        adapter
            .control(
                &exchange,
                request.adapter_request_id,
                ProviderStreamControlAction::Cancel,
            )
            .expect("pre-open Cancel no-op");
        adapter
            .control(
                &exchange,
                request.adapter_request_id,
                ProviderStreamControlAction::Cancel,
            )
            .expect("exact Cancel replay");
        adapter
            .control(
                &exchange,
                request.adapter_request_id,
                ProviderStreamControlAction::Release,
            )
            .expect("pre-open Release no-op");
        assert_eq!(
            adapter
                .open_https(request, SECRET)
                .expect_err("late open must remain fenced"),
            ProviderAdapterError::rejected()
        );
        let foreign = ModelExchangeId("mdl_00000000000000000000000004".to_owned());
        assert_eq!(
            adapter
                .control(
                    &foreign,
                    request.adapter_request_id,
                    ProviderStreamControlAction::Release,
                )
                .expect_err("foreign control"),
            ProviderAdapterError::protocol()
        );
        assert!(fixture.finish().is_empty());
    }

    #[test]
    fn configuration_requires_https_and_verification_limits() {
        let result = HttpsSseProviderConfig::try_new(
            "provider".to_owned(),
            "http://localhost/v1/model".to_owned(),
            HttpsSseProviderTimeouts {
                connect: Duration::from_secs(1),
                first_byte: Duration::from_secs(1),
                idle: Duration::from_secs(1),
                total: Duration::from_secs(2),
            },
            HttpsSseProviderLimits {
                response_bytes: 1024,
                event_bytes: 1024,
                events: 1,
            },
        );
        assert_eq!(
            result.expect_err("plaintext endpoint").kind(),
            HttpsSseProviderErrorKind::InvalidConfiguration
        );
        let result = HttpsSseProviderConfig::try_new(
            "provider".to_owned(),
            "https://user:secret@localhost/v1/model".to_owned(),
            HttpsSseProviderTimeouts {
                connect: Duration::from_secs(1),
                first_byte: Duration::from_secs(1),
                idle: Duration::from_secs(1),
                total: Duration::from_secs(2),
            },
            HttpsSseProviderLimits {
                response_bytes: 1024,
                event_bytes: 1024,
                events: 1,
            },
        );
        assert_eq!(
            result.expect_err("credential-bearing endpoint").kind(),
            HttpsSseProviderErrorKind::InvalidConfiguration
        );
    }

    #[test]
    fn chatgpt_responses_protocol_is_pinned_and_uses_the_stored_credential_shape() {
        assert!(provider_event_stream_content_type(
            HttpsSseProviderProtocol::OpenAiChatGptResponses,
            None,
        ));
        assert!(!provider_event_stream_content_type(
            HttpsSseProviderProtocol::Canonical,
            None,
        ));
        assert!(provider_event_stream_content_type(
            HttpsSseProviderProtocol::OpenAiChatGptResponses,
            Some("text/event-stream; charset=utf-8"),
        ));
        assert!(!provider_event_stream_content_type(
            HttpsSseProviderProtocol::OpenAiChatGptResponses,
            Some("application/json"),
        ));
        let timeouts = HttpsSseProviderTimeouts {
            connect: Duration::from_secs(1),
            first_byte: Duration::from_secs(1),
            idle: Duration::from_secs(1),
            total: Duration::from_secs(2),
        };
        let limits = HttpsSseProviderLimits {
            response_bytes: 4096,
            event_bytes: 2048,
            events: 16,
        };
        let wrong_endpoint = HttpsSseProviderConfig::try_new(
            "openai".to_owned(),
            "https://api.openai.com/v1/responses".to_owned(),
            timeouts,
            limits,
        )
        .expect("valid generic HTTPS configuration");
        assert_eq!(
            wrong_endpoint
                .with_openai_chatgpt_responses()
                .expect_err("ChatGPT credential cannot be redirected")
                .kind(),
            HttpsSseProviderErrorKind::InvalidConfiguration
        );
        HttpsSseProviderConfig::try_new(
            "openai".to_owned(),
            OPENAI_CHATGPT_RESPONSES_ENDPOINT.to_owned(),
            timeouts,
            limits,
        )
        .expect("pinned endpoint configuration")
        .with_openai_chatgpt_responses()
        .expect("pinned ChatGPT protocol");

        let secret = br#"{
            "kind":"credential",
            "schema":"winwincode.provider-credential.v1",
            "accessToken":"ACCESS_TOKEN",
            "refreshToken":"REFRESH_TOKEN",
            "idToken":"ID_TOKEN",
            "accountId":"ACCOUNT_ID",
            "expiresAtMillis":1893456000000
        }"#;
        let authentication = openai_authentication(secret).expect("stored credential");
        assert_eq!(authentication.authorization, "Bearer ACCESS_TOKEN");
        assert_eq!(authentication.account_id.as_deref(), Some("ACCOUNT_ID"));

        let request = prepare_openai_responses_request(
            br#"{"model":"caller-model","input":"hello","stream":false}"#,
            "gpt-account-model",
        )
        .expect("Responses request");
        let request: serde_json::Value = serde_json::from_slice(&request).expect("request JSON");
        assert_eq!(request["model"], "gpt-account-model");
        assert_eq!(request["stream"], true);
        assert_eq!(request["input"], "hello");
    }

    #[test]
    fn chatgpt_responses_sse_maps_text_and_usage_to_the_canonical_stream() {
        let stream = concat!(
            "data: {\"type\":\"response.created\",\"response\":{\"id\":\"resp_1\"}}\n\n",
            "data: {\"type\":\"response.output_item.added\",\"output_index\":0,\"item\":{\"type\":\"message\"}}\n\n",
            "data: {\"type\":\"response.output_text.delta\",\"output_index\":0,\"delta\":\"hello\"}\n\n",
            "data: {\"type\":\"response.output_text.done\",\"output_index\":0}\n\n",
            "data: {\"type\":\"response.completed\",\"response\":{\"id\":\"resp_1\",\"usage\":{\"input_tokens\":11,\"input_tokens_details\":{\"cached_tokens\":2},\"output_tokens\":5,\"output_tokens_details\":{\"reasoning_tokens\":3}}}}\n\n",
            "data: [DONE]\n\n",
        );
        let parsed = parse_openai_responses_sse(stream.as_bytes(), 4096, 16)
            .expect("OpenAI Responses stream");
        assert!(matches!(
            parsed.events.first(),
            Some(ProviderStreamEvent::ResponseStarted { provider_response_id })
                if provider_response_id == "resp_1"
        ));
        assert!(parsed.events.contains(&ProviderStreamEvent::TextDelta {
            index: 0,
            delta: "hello".to_owned(),
        }));
        assert!(matches!(
            parsed.terminal,
            ProviderGatewayTerminal::Completed {
                usage: ProviderTokenUsage {
                    input_tokens: 11,
                    cached_input_tokens: 2,
                    cache_write_input_tokens: 0,
                    output_tokens: 5,
                    reasoning_output_tokens: 3,
                },
                actual_cost_micros: 0,
            }
        ));
    }
}
