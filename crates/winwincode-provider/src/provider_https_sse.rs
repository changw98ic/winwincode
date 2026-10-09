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

use serde::Deserialize;
use sha2::{Digest, Sha256};
use ureq::unversioned::transport::{ConnectProxyConnector, Connector as _, RustlsConnector};
use winwincode_domain::{ModelExchangeId, RequestId};

use crate::provider_anthropic::{
    AnthropicCodecErrorKind, AnthropicMessagesOptions, AnthropicToolBindings,
    PreparedAnthropicRequest, ProviderTokenPricing, parse_anthropic_sse, prepare_anthropic_request,
};
use crate::{
    CanonicalModelStreamFrame, ModelAttemptFailureFact, ModelExecutionCertainty,
    ProviderAdapterError, ProviderAdapterInvocation, ProviderAdapterOpenReceipt,
    ProviderAdapterPort, ProviderFailureDiagnostic, ProviderFailureMetadata, ProviderFinishReason,
    ProviderGatewayOpenReceipt, ProviderGatewayTerminal, ProviderStreamControlAction,
    ProviderStreamConverter, ProviderStreamEvent, ProviderStreamFailure, ProviderStreamFailureKind,
    ProviderTokenUsage, ProviderToolIdentity, ProviderToolKind, ResolvedSecret,
};

const MAX_SAFE_INTEGER: u64 = 9_007_199_254_740_991;
const MAX_PROVIDER_ID_BYTES: usize = 128;
/// Authority-owned endpoint size bound, shared with the Provider modules that
/// must accept exactly the endpoints this adapter accepts.
pub const MAX_ENDPOINT_BYTES: usize = 2_048;
const MAX_ADAPTER_REQUEST_ID_BYTES: usize = 200;
const AUTHORIZATION_PREFIX: &[u8] = b"Bearer ";
const CONTROL_PAUSE: u8 = 1;
const CONTROL_RESUME: u8 = 2;
const CONTROL_CANCEL: u8 = 4;
const CONTROL_RELEASE: u8 = 8;

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
#[derive(Clone)]
pub struct HttpsSseProviderConfig {
    provider_id: String,
    endpoint: String,
    connect_timeout: Duration,
    idle_timeout: Duration,
    total_timeout: Duration,
    deadlines_enabled: bool,
    max_response_bytes: usize,
    max_event_bytes: usize,
    max_events: usize,
    tls_roots: ProviderTlsRoots,
    protocol: HttpsSseProviderProtocol,
    custom_headers: Vec<(String, String)>,
    codex_account_id: Option<String>,
    http_connect_proxy: Option<ureq::Proxy>,
}

impl fmt::Debug for HttpsSseProviderConfig {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("HttpsSseProviderConfig")
            .field("provider_id", &self.provider_id)
            .field("endpoint", &"[REDACTED]")
            .field("protocol", &self.protocol)
            .field(
                "http_connect_proxy",
                &self.http_connect_proxy.as_ref().map(|_| "[REDACTED]"),
            )
            .field(
                "custom_headers",
                &self
                    .custom_headers
                    .iter()
                    .map(|(name, _)| name.as_str())
                    .collect::<Vec<_>>(),
            )
            .finish_non_exhaustive()
    }
}

#[derive(Clone, Copy, Debug)]
enum HttpsSseProviderProtocol {
    Canonical,
    CodexChatgpt,
    ChatgptPlan,
    OpenAiResponses {
        max_output_tokens: u32,
        structured_output: ResponsesStructuredOutput,
    },
    AnthropicMessages(AnthropicMessagesOptions),
    OpenAiChatCompletions(AnthropicMessagesOptions),
}

#[derive(Clone, Copy, Debug)]
enum ResponsesStructuredOutput {
    JsonSchema,
    JsonObject,
    Text,
}

/// Maximum number of extra request headers accepted on one Provider.
pub const MAX_CUSTOM_HEADERS: usize = 32;
/// Maximum custom header name length in bytes.
pub const MAX_CUSTOM_HEADER_NAME_BYTES: usize = 128;
/// Maximum custom header value length in bytes.
pub const MAX_CUSTOM_HEADER_VALUE_BYTES: usize = 2_048;

/// Hop-by-hop and credential-bearing headers that custom headers may never set.
const FORBIDDEN_CUSTOM_HEADERS: &[&str] = &[
    "authorization",
    "content-type",
    "accept",
    "idempotency-key",
    "x-winwincode-model",
    "anthropic-version",
    "host",
    "content-length",
    "connection",
    "keep-alive",
    "proxy-authenticate",
    "proxy-authorization",
    "te",
    "trailer",
    "transfer-encoding",
    "upgrade",
];

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
            idle_timeout: timeouts.idle,
            total_timeout: timeouts.total,
            deadlines_enabled: true,
            max_response_bytes: limits.response_bytes,
            max_event_bytes: limits.event_bytes,
            max_events: limits.events,
            tls_roots: ProviderTlsRoots::WebPki,
            protocol: HttpsSseProviderProtocol::Canonical,
            custom_headers: Vec::new(),
            codex_account_id: None,
            http_connect_proxy: None,
        };
        config.validate()?;
        Ok(config)
    }

    /// Routes verified HTTPS through one explicitly configured HTTP CONNECT proxy.
    /// No ambient proxy environment variables are read by this configuration.
    ///
    /// # Errors
    /// Rejects malformed URLs, unsupported proxy protocols and URL paths or queries.
    pub fn with_http_connect_proxy(
        mut self,
        proxy_url: &str,
    ) -> Result<Self, HttpsSseProviderError> {
        let invalid =
            || HttpsSseProviderError::new(HttpsSseProviderErrorKind::InvalidConfiguration);
        if !valid_token(proxy_url, MAX_ENDPOINT_BYTES) || proxy_url.contains('#') {
            return Err(invalid());
        }
        let uri = ureq::http::Uri::from_str(proxy_url).map_err(|_| invalid())?;
        if uri.scheme_str() != Some("http")
            || !uri.authority().is_some_and(|authority| {
                !authority.host().is_empty()
                    && (authority.as_str().rsplit('@').next() == Some(authority.host())
                        || authority.port_u16().is_some())
            })
            || !uri
                .path_and_query()
                .is_none_or(|path| matches!(path.path(), "" | "/") && path.query().is_none())
        {
            return Err(invalid());
        }
        self.http_connect_proxy = Some(ureq::Proxy::new(proxy_url).map_err(|_| invalid())?);
        Ok(self)
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

    /// Selects the `OpenAI` chat/completions request and `chat.completion.chunk`
    /// streaming protocol. The configured endpoint is used verbatim.
    ///
    /// # Errors
    ///
    /// Rejects zero output limits or unsafe token pricing.
    pub fn with_openai_chat_completions(
        mut self,
        max_output_tokens: u32,
        pricing: ProviderTokenPricing,
    ) -> Result<Self, HttpsSseProviderError> {
        let options = AnthropicMessagesOptions {
            max_output_tokens,
            pricing,
        };
        options.validate().map_err(map_anthropic_configuration)?;
        self.protocol = HttpsSseProviderProtocol::OpenAiChatCompletions(options);
        self.validate()?;
        Ok(self)
    }

    /// Selects the native Responses API with a bounded output and ordinary API credentials.
    /// The configured HTTPS endpoint and extra headers remain authoritative.
    ///
    /// # Errors
    /// Rejects a zero output limit or invalid transport configuration.
    pub fn with_openai_responses(
        mut self,
        max_output_tokens: u32,
    ) -> Result<Self, HttpsSseProviderError> {
        self.protocol = HttpsSseProviderProtocol::OpenAiResponses {
            max_output_tokens,
            structured_output: ResponsesStructuredOutput::JsonSchema,
        };
        self.validate()?;
        Ok(self)
    }

    /// Selects Responses JSON-object compatibility for structured final output.
    ///
    /// # Errors
    /// Rejects invalid transport configuration or a zero output limit.
    pub fn with_openai_responses_json_object(
        mut self,
        max_output_tokens: u32,
    ) -> Result<Self, HttpsSseProviderError> {
        self.protocol = HttpsSseProviderProtocol::OpenAiResponses {
            max_output_tokens,
            structured_output: ResponsesStructuredOutput::JsonObject,
        };
        self.validate()?;
        Ok(self)
    }

    /// Selects native Responses text with local structured final-output validation.
    ///
    /// # Errors
    /// Rejects invalid transport configuration or a zero output limit.
    pub fn with_openai_responses_text(
        mut self,
        max_output_tokens: u32,
    ) -> Result<Self, HttpsSseProviderError> {
        self.protocol = HttpsSseProviderProtocol::OpenAiResponses {
            max_output_tokens,
            structured_output: ResponsesStructuredOutput::Text,
        };
        self.validate()?;
        Ok(self)
    }

    /// Replaces any previous extra request headers with a bounded validated set.
    ///
    /// # Errors
    ///
    /// Rejects more than [`MAX_CUSTOM_HEADERS`] entries, names or values outside
    /// their bounds, CR/LF in values, and hop-by-hop or credential-bearing
    /// header names (`Authorization`, `Host`, `Content-Length`, `Connection`, …).
    pub fn with_custom_headers(
        mut self,
        headers: impl IntoIterator<Item = (String, String)>,
    ) -> Result<Self, HttpsSseProviderError> {
        let headers: Vec<(String, String)> = headers.into_iter().collect();
        validate_custom_headers(&headers)?;
        self.custom_headers = headers;
        self.validate()?;
        Ok(self)
    }

    /// Lets an active task keep a provider stream open until it completes or is cancelled.
    #[must_use]
    pub fn without_deadlines(mut self) -> Self {
        self.deadlines_enabled = false;
        self
    }

    /// Selects the fixed `ChatGPT` Codex endpoint with an account-bound OAuth token.
    ///
    /// # Errors
    /// Rejects custom endpoints and invalid account header values.
    pub fn with_codex_chatgpt(mut self, account_id: String) -> Result<Self, HttpsSseProviderError> {
        if self.endpoint != crate::codex_login::CODEX_ENDPOINT || !valid_token(&account_id, 200) {
            return Err(HttpsSseProviderError::new(
                HttpsSseProviderErrorKind::InvalidConfiguration,
            ));
        }
        self.protocol = HttpsSseProviderProtocol::CodexChatgpt;
        self.codex_account_id = Some(account_id);
        self.validate()?;
        Ok(self)
    }

    /// Selects the official public Responses endpoint for a `WinWinCode` OAuth grant.
    ///
    /// # Errors
    /// Rejects any custom endpoint to keep the account credential on its intended origin.
    pub fn with_chatgpt_plan(mut self) -> Result<Self, HttpsSseProviderError> {
        if self.endpoint != crate::chatgpt_oauth::ENDPOINT {
            return Err(HttpsSseProviderError::new(
                HttpsSseProviderErrorKind::InvalidConfiguration,
            ));
        }
        self.protocol = HttpsSseProviderProtocol::ChatgptPlan;
        self.codex_account_id = None;
        self.validate()?;
        Ok(self)
    }

    fn validate(&self) -> Result<(), HttpsSseProviderError> {
        if !valid_token(&self.provider_id, MAX_PROVIDER_ID_BYTES)
            || self.endpoint.len() > MAX_ENDPOINT_BYTES
            || !canonical_https_endpoint(&self.endpoint)
            || self.connect_timeout.is_zero()
            || self.idle_timeout.is_zero()
            || self.total_timeout.is_zero()
            || self.connect_timeout > self.total_timeout
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
        validate_custom_headers(&self.custom_headers)?;
        if matches!(
            self.protocol,
            HttpsSseProviderProtocol::CodexChatgpt | HttpsSseProviderProtocol::ChatgptPlan
        ) && !self.custom_headers.is_empty()
        {
            return Err(HttpsSseProviderError::new(
                HttpsSseProviderErrorKind::InvalidConfiguration,
            ));
        }
        match self.protocol {
            HttpsSseProviderProtocol::AnthropicMessages(options)
            | HttpsSseProviderProtocol::OpenAiChatCompletions(options) => {
                options.validate().map_err(map_anthropic_configuration)?;
            }
            HttpsSseProviderProtocol::Canonical
            | HttpsSseProviderProtocol::CodexChatgpt
            | HttpsSseProviderProtocol::ChatgptPlan => {}
            HttpsSseProviderProtocol::OpenAiResponses {
                max_output_tokens, ..
            } => {
                if max_output_tokens == 0 {
                    return Err(HttpsSseProviderError::new(
                        HttpsSseProviderErrorKind::InvalidConfiguration,
                    ));
                }
            }
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
    SseFraming,
    SseEvent,
    IncompleteStream,
    StreamConversion,
    SizeLimit,
    Paused,
    CredentialLeak,
}

/// Secret-free HTTPS/SSE error.
#[derive(Clone, Eq, PartialEq)]
pub struct HttpsSseProviderError {
    kind: HttpsSseProviderErrorKind,
    metadata: Box<ProviderFailureMetadata>,
    response: Option<Vec<u8>>,
    observed_receipt: Option<Box<(String, ProviderTokenUsage)>>,
    network: Option<Box<winwincode_network::NetworkFailure>>,
}

impl fmt::Debug for HttpsSseProviderError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("HttpsSseProviderError")
            .field("kind", &self.kind)
            .field("metadata", &self.metadata)
            .field("network", &self.network)
            .field("response", &self.response.as_ref().map(|_| "[REDACTED]"))
            .field(
                "observed_receipt",
                &self.observed_receipt.as_deref().map(|(_, usage)| usage),
            )
            .finish()
    }
}

impl HttpsSseProviderError {
    pub(crate) fn new(kind: HttpsSseProviderErrorKind) -> Self {
        Self {
            kind,
            metadata: Box::default(),
            response: None,
            observed_receipt: None,
            network: None,
        }
    }

    #[must_use]
    pub const fn kind(&self) -> HttpsSseProviderErrorKind {
        self.kind
    }

    #[must_use]
    pub fn metadata(&self) -> &ProviderFailureMetadata {
        &self.metadata
    }

    #[must_use]
    pub fn retryable(&self) -> bool {
        self.network_failure().retryable()
    }

    #[must_use]
    pub fn network_failure(&self) -> winwincode_network::NetworkFailure {
        use winwincode_network::{Acceptance, ErrorKind, NetworkFailure, Phase};
        self.network.as_deref().copied().unwrap_or_else(|| {
            if let Some(status) = self.metadata.status.filter(|status| *status >= 400) {
                return NetworkFailure::http(
                    status,
                    self.metadata
                        .provider_retry_after_millis
                        .map(Duration::from_millis),
                );
            }
            let mut failure = NetworkFailure::new(
                match self.kind {
                    HttpsSseProviderErrorKind::Transport => ErrorKind::TransportInterrupted,
                    HttpsSseProviderErrorKind::IncompleteStream => ErrorKind::StreamIncomplete,
                    HttpsSseProviderErrorKind::RateLimited => ErrorKind::RateLimited,
                    HttpsSseProviderErrorKind::SseFraming
                    | HttpsSseProviderErrorKind::SseEvent
                    | HttpsSseProviderErrorKind::StreamConversion
                    | HttpsSseProviderErrorKind::SizeLimit => ErrorKind::ProtocolInvalid,
                    HttpsSseProviderErrorKind::CredentialLeak
                    | HttpsSseProviderErrorKind::IdentityConflict => ErrorKind::IntegrityInvalid,
                    HttpsSseProviderErrorKind::Paused => ErrorKind::Cancelled,
                    HttpsSseProviderErrorKind::Unavailable => ErrorKind::StorageUnavailable,
                    _ => ErrorKind::RequestInvalid,
                },
                Acceptance::ResponseReceived,
                Phase::Stream,
            );
            failure.http_status = self.metadata.status;
            failure.retry_after_ms = self.metadata.provider_retry_after_millis;
            failure.diagnostic = Some(winwincode_network::NetworkDiagnostic::new(
                match self.kind {
                    HttpsSseProviderErrorKind::SseFraming => {
                        winwincode_network::DiagnosticCode::SseFraming
                    }
                    HttpsSseProviderErrorKind::SseEvent => {
                        winwincode_network::DiagnosticCode::SseEvent
                    }
                    HttpsSseProviderErrorKind::StreamConversion => {
                        winwincode_network::DiagnosticCode::StreamConversion
                    }
                    HttpsSseProviderErrorKind::IncompleteStream => {
                        winwincode_network::DiagnosticCode::StreamIncomplete
                    }
                    HttpsSseProviderErrorKind::SizeLimit => {
                        winwincode_network::DiagnosticCode::BodyTooLarge
                    }
                    _ => winwincode_network::DiagnosticCode::TransportOther,
                },
            ));
            failure
        })
    }

    pub(crate) fn response(&self) -> Option<&[u8]> {
        self.response.as_deref()
    }

    pub(crate) fn observed_receipt(&self) -> Option<&(String, ProviderTokenUsage)> {
        self.observed_receipt.as_deref()
    }

    fn with_response(mut self, response: Vec<u8>) -> Self {
        self.response = Some(response);
        self
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
            HttpsSseProviderErrorKind::SseFraming
            | HttpsSseProviderErrorKind::SseEvent
            | HttpsSseProviderErrorKind::IncompleteStream
            | HttpsSseProviderErrorKind::StreamConversion
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

struct AttemptDone<'a>(&'a std::sync::atomic::AtomicBool);
impl Drop for AttemptDone<'_> {
    fn drop(&mut self) {
        self.0.store(true, std::sync::atomic::Ordering::Release);
    }
}

struct SharedAdapter {
    config: HttpsSseProviderConfig,
    streams: Mutex<BTreeMap<String, StreamRecord>>,
    sse_failure_log: Mutex<Option<std::path::PathBuf>>,
    journal: Mutex<Option<winwincode_network::journal::RequestJournal>>,
    completions: Mutex<BTreeMap<String, HttpsSseProviderCompletion>>,
    cancellation: Mutex<Option<Arc<crate::provider_transport::ExchangeCancellation>>>,
}

struct StreamRecord {
    model_exchange_id: ModelExchangeId,
    invocation_digest: [u8; 32],
    controls: u8,
    tool_bindings: AnthropicToolBindings,
    response_schema: Option<crate::provider_response_schema::ResponseSchema>,
    state: StreamState,
    io: Arc<crate::provider_transport::ExchangeIo>,
    metadata: ProviderFailureMetadata,
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
    #[allow(
        clippy::too_many_lines,
        reason = "one durable attempt ownership loop keeps prepare, external effect and commit ordered"
    )]
    pub(crate) fn open_recovering_admitted<G>(
        &self,
        invocation: &ProviderAdapterInvocation<'_>,
        credential: &ResolvedSecret,
        receipt: &ProviderGatewayOpenReceipt,
        can_start: &(dyn Fn() -> bool + Sync),
        mut admit: impl FnMut() -> Result<G, ProviderAdapterError>,
    ) -> Result<ProviderAdapterOpenReceipt, ProviderAdapterError> {
        use winwincode_network::{
            Acceptance, ErrorKind, NetworkFailure, Phase, Replay,
            journal::{QueuePermit, now_millis},
        };
        let storage = || {
            ProviderAdapterError::from_network(NetworkFailure::new(
                ErrorKind::StorageUnavailable,
                Acceptance::NotSent,
                Phase::Persist,
            ))
        };
        let journal = self
            .shared
            .journal
            .lock()
            .map_err(|_| storage())?
            .clone()
            .ok_or_else(storage)?;
        let operation = invocation_digest(HttpInvocation::from_adapter(invocation));
        let cached = self
            .shared
            .completions
            .lock()
            .map_err(|_| storage())?
            .contains_key(invocation.adapter_request_id());
        if cached {
            return self.open(invocation, credential);
        }
        let started = std::time::Instant::now();
        let live = || {
            can_start()
                && (!self.shared.config.deadlines_enabled
                    || started.elapsed() < self.shared.config.total_timeout)
        };
        let stopped = || {
            ProviderAdapterError::from_network(NetworkFailure::new(
                if can_start() {
                    ErrorKind::Timeout
                } else {
                    ErrorKind::AuthorityExpired
                },
                Acceptance::Unknown,
                Phase::Stream,
            ))
        };
        loop {
            if !live() {
                return Err(stopped());
            }
            let sequence = match journal
                .prepare(
                    &operation,
                    Replay::RetryInference,
                    winwincode_network::defaults().max_attempts,
                    now_millis(),
                )
                .map_err(ProviderAdapterError::from_network)?
            {
                QueuePermit::Ready(sequence) => sequence,
                QueuePermit::Stopped(failure) => {
                    return Err(ProviderAdapterError::from_network(failure));
                }
                QueuePermit::Waiting(delay, _) => {
                    let until = std::time::Instant::now() + delay;
                    while let Some(remaining) =
                        until.checked_duration_since(std::time::Instant::now())
                    {
                        if !live() {
                            return Err(stopped());
                        }
                        std::thread::sleep(remaining.min(Duration::from_millis(
                            winwincode_network::defaults().authority_check_ms,
                        )));
                    }
                    continue;
                }
            };
            let permit = match admit() {
                Ok(permit) => permit,
                Err(error) => {
                    journal
                        .finish(
                            &operation,
                            sequence,
                            Some(error.network_failure()),
                            now_millis(),
                        )
                        .map_err(ProviderAdapterError::from_network)?;
                    return Err(error);
                }
            };
            let result = self.recovering_attempt(invocation, credential, receipt, &live);
            drop(permit);
            let failure = result
                .as_ref()
                .err()
                .map(ProviderAdapterError::network_failure);
            journal
                .finish(&operation, sequence, failure, now_millis())
                .map_err(ProviderAdapterError::from_network)?;
            match result {
                Ok((ack, mut completion)) => {
                    if journal
                        .has_unknown_usage(&operation)
                        .map_err(ProviderAdapterError::from_network)?
                        && let ProviderGatewayTerminal::Completed {
                            actual_cost_micros, ..
                        } = &mut completion.terminal
                    {
                        *actual_cost_micros = None;
                    }
                    self.shared
                        .completions
                        .lock()
                        .map_err(|_| storage())?
                        .insert(invocation.adapter_request_id().to_owned(), completion);
                    return Ok(ack);
                }
                Err(error) => {
                    // Only an exact failed drain can reopen. Terminal controls remain fences.
                    let mut streams = self.shared.streams.lock().map_err(|_| storage())?;
                    if let Some(record) = streams.get(invocation.adapter_request_id())
                        && matches!(record.state, StreamState::Drained)
                        && record.model_exchange_id == *invocation.model_exchange_id()
                        && record.invocation_digest == operation
                    {
                        streams.remove(invocation.adapter_request_id());
                    }
                    if !error.network_failure().retryable() {
                        return Err(error);
                    }
                }
            }
        }
    }

    fn recovering_attempt(
        &self,
        invocation: &ProviderAdapterInvocation<'_>,
        credential: &ResolvedSecret,
        receipt: &ProviderGatewayOpenReceipt,
        live: &(dyn Fn() -> bool + Sync),
    ) -> Result<(ProviderAdapterOpenReceipt, HttpsSseProviderCompletion), ProviderAdapterError>
    {
        let done = std::sync::atomic::AtomicBool::new(false);
        std::thread::scope(|scope| {
            scope.spawn(|| {
                while !done.load(std::sync::atomic::Ordering::Acquire) {
                    if !live()
                        && let Ok(streams) = self.shared.streams.lock()
                        && let Some(record) = streams.get(invocation.adapter_request_id())
                    {
                        record.io.cancel();
                    }
                    // Observe until open has installed its cancellable record.
                    std::thread::sleep(Duration::from_millis(
                        winwincode_network::defaults().authority_check_ms,
                    ));
                }
            });
            let _done = AttemptDone(&done);
            self.open(invocation, credential).and_then(|ack| {
                self.drain_attempt(receipt)
                    .map(|completion| (ack, completion))
                    .map_err(|error| ProviderAdapterError::from_network(error.network_failure()))
            })
        })
    }

    fn agent(
        config: &HttpsSseProviderConfig,
        io: Arc<crate::provider_transport::ExchangeIo>,
    ) -> ureq::Agent {
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
        let agent_config = ureq::Agent::config_builder()
            .http_status_as_error(false)
            .max_redirects(0)
            .proxy(config.http_connect_proxy.clone())
            .timeout_connect(Some(config.connect_timeout))
            // ureq 3.1.4 also applies recv_response's deadline while reading the body.
            // When enabled, the global deadline bounds headers and the complete stream.
            .timeout_resolve(Some(config.connect_timeout))
            .timeout_global(config.deadlines_enabled.then_some(config.total_timeout))
            .tls_config(tls)
            .build();
        // CONNECT recursively opens its proxy through this same chain without
        // proxy settings. ExchangeConnector retains that TCP socket for real
        // cancellation, then preserves the tunnel for target TLS verification.
        let connector = ConnectProxyConnector::default()
            .chain(crate::provider_transport::ExchangeConnector(io))
            .chain(RustlsConnector::default());
        ureq::Agent::with_parts(
            agent_config,
            connector,
            ureq::unversioned::resolver::DefaultResolver::default(),
        )
    }

    /// Builds a verified HTTP agent from one validated, explicitly routed configuration.
    ///
    /// # Errors
    ///
    /// Rejects malformed explicit TLS roots or transport configuration.
    pub fn try_new(config: HttpsSseProviderConfig) -> Result<Self, HttpsSseProviderError> {
        config.validate()?;
        Ok(Self {
            shared: Arc::new(SharedAdapter {
                config,
                streams: Mutex::new(BTreeMap::new()),
                sse_failure_log: Mutex::new(None),
                journal: Mutex::new(None),
                completions: Mutex::new(BTreeMap::new()),
                cancellation: Mutex::new(None),
            }),
        })
    }

    /// Attaches the durable logical-request attempt journal.
    #[must_use]
    pub fn with_journal(self, journal: winwincode_network::journal::RequestJournal) -> Self {
        if let Ok(mut current) = self.shared.journal.lock() {
            *current = Some(journal);
        }
        self
    }

    /// Retains private failed response bytes with a payload-free log reference.
    #[must_use]
    pub fn with_sse_failure_log(self, directory: std::path::PathBuf) -> Self {
        if let Ok(mut current) = self.shared.sse_failure_log.lock() {
            *current = Some(directory);
        }
        self
    }

    pub(crate) fn with_cancellation(
        self,
        cancellation: Arc<crate::provider_transport::ExchangeCancellation>,
    ) -> Self {
        if let Ok(mut current) = self.shared.cancellation.lock() {
            *current = Some(cancellation);
        }
        self
    }
    fn exchange_io(&self) -> Arc<crate::provider_transport::ExchangeIo> {
        let io = crate::provider_transport::ExchangeIo::new(
            self.shared.config.connect_timeout,
            self.shared.config.idle_timeout,
        );
        if let Ok(current) = self.shared.cancellation.lock()
            && let Some(current) = current.as_ref()
        {
            current.attach(&io);
        }
        io
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
        if let Some(completion) = self
            .shared
            .completions
            .lock()
            .map_err(|_| HttpsSseProviderError::new(HttpsSseProviderErrorKind::Unavailable))?
            .remove(&receipt.adapter_request_id)
        {
            return Ok(completion);
        }
        self.drain_attempt(receipt)
    }

    fn drain_attempt(
        &self,
        receipt: &ProviderGatewayOpenReceipt,
    ) -> Result<HttpsSseProviderCompletion, HttpsSseProviderError> {
        let mut body = self.take_body(receipt)?;
        let tool_bindings = self.stream_tool_bindings(receipt)?;
        let response_schema = self.stream_response_schema(receipt)?;
        let result = self
            .convert_response(receipt, &mut body, &tool_bindings, response_schema.as_ref())
            .map_err(|mut error| {
                if let Ok(streams) = self.shared.streams.lock()
                    && let Some(record) = streams.get(&receipt.adapter_request_id)
                {
                    let diagnostic = error.metadata.diagnostic.take();
                    error.metadata = Box::new(record.metadata.clone());
                    error.metadata.diagnostic = diagnostic;
                }
                let mut failure = error.network_failure();
                if let Some(bytes) = error.response.as_deref() {
                    failure = self.retain_failed_response(
                        failure,
                        &serde_json::json!({
                            "schemaVersion": 1,
                            "failure": failure,
                            "providerDiagnostic": error.metadata.diagnostic,
                        }),
                        bytes,
                    );
                }
                error.network = Some(Box::new(failure));
                error
            });
        self.finish_drain(receipt)?;
        result
    }

    fn convert_response(
        &self,
        receipt: &ProviderGatewayOpenReceipt,
        body: &mut ureq::Body,
        tool_bindings: &AnthropicToolBindings,
        schema: Option<&crate::provider_response_schema::ResponseSchema>,
    ) -> Result<HttpsSseProviderCompletion, HttpsSseProviderError> {
        let mut receipt = receipt.clone();
        for (name, value) in &self.shared.config.custom_headers {
            // This public protocol switch is a literal boolean, not a credential.
            if matches!(
                self.shared.config.protocol,
                HttpsSseProviderProtocol::OpenAiResponses { .. }
            ) && name.eq_ignore_ascii_case("x-openai-internal-codex-responses-lite")
                && value == "true"
            {
                continue;
            }
            let secret = ResolvedSecret::from_bytes(value.as_bytes().to_vec()).map_err(|_| {
                HttpsSseProviderError::new(HttpsSseProviderErrorKind::InvalidConfiguration)
            })?;
            receipt.stream_leak_gate.track_secret(&secret);
        }
        let receipt = &receipt;
        let bytes = match self.read_bounded(receipt, body) {
            Ok(bytes) => bytes,
            Err(mut error) => {
                if let Some(bytes) = error.response.as_deref() {
                    error.observed_receipt = self.observed_receipt(bytes, receipt).map(Box::new);
                    let cancelled = self.shared.cancellation.lock().map_or(true, |value| {
                        value
                            .as_ref()
                            .is_some_and(|cancellation| cancellation.is_cancelled())
                    });
                    if matches!(
                        self.shared.config.protocol,
                        HttpsSseProviderProtocol::OpenAiChatCompletions(_)
                            | HttpsSseProviderProtocol::OpenAiResponses { .. }
                            | HttpsSseProviderProtocol::CodexChatgpt
                            | HttpsSseProviderProtocol::ChatgptPlan
                    ) && error.kind == HttpsSseProviderErrorKind::Transport
                        && !cancelled
                        && !self.drain_interrupted(receipt)?
                        && let Ok(completion) =
                            self.convert_bytes(receipt, tool_bindings, bytes, schema)
                        && matches!(
                            completion.terminal,
                            ProviderGatewayTerminal::Completed { .. }
                                | ProviderGatewayTerminal::Failed { .. }
                        )
                    {
                        return Ok(completion);
                    }
                }
                return Err(error);
            }
        };
        self.convert_bytes(receipt, tool_bindings, &bytes, schema)
            .map_err(|mut error| {
                error.observed_receipt = self.observed_receipt(&bytes, receipt).map(Box::new);
                error.with_response(bytes)
            })
    }

    fn convert_bytes(
        &self,
        receipt: &ProviderGatewayOpenReceipt,
        tool_bindings: &AnthropicToolBindings,
        bytes: &[u8],
        schema: Option<&crate::provider_response_schema::ResponseSchema>,
    ) -> Result<HttpsSseProviderCompletion, HttpsSseProviderError> {
        if matches!(
            self.shared.config.protocol,
            HttpsSseProviderProtocol::OpenAiResponses { .. }
        ) {
            return crate::provider_codex::parse_api_response_with_schema(
                bytes,
                receipt,
                self.shared.config.max_event_bytes,
                self.shared.config.max_events,
                schema,
            );
        }
        if matches!(
            self.shared.config.protocol,
            HttpsSseProviderProtocol::CodexChatgpt | HttpsSseProviderProtocol::ChatgptPlan
        ) {
            return crate::provider_codex::parse_response(
                bytes,
                receipt,
                self.shared.config.max_event_bytes,
                self.shared.config.max_events,
            );
        }
        let (events, terminal) = match self.shared.config.protocol {
            HttpsSseProviderProtocol::CodexChatgpt
            | HttpsSseProviderProtocol::ChatgptPlan
            | HttpsSseProviderProtocol::OpenAiResponses { .. } => {
                unreachable!("Responses protocols return above")
            }
            HttpsSseProviderProtocol::Canonical => {
                let parsed = parse_sse(
                    bytes,
                    self.shared.config.max_event_bytes,
                    self.shared.config.max_events,
                )?;
                (parsed.events, parsed.terminal)
            }
            HttpsSseProviderProtocol::AnthropicMessages(options) => {
                let parsed = parse_anthropic_sse(
                    bytes,
                    self.shared.config.max_event_bytes,
                    self.shared.config.max_events,
                    tool_bindings,
                    options,
                )
                .map_err(map_anthropic_response)?;
                (parsed.events, parsed.terminal)
            }
            HttpsSseProviderProtocol::OpenAiChatCompletions(options) => {
                let parsed = crate::provider_openai::parse_openai_chat_sse(
                    bytes,
                    self.shared.config.max_event_bytes,
                    self.shared.config.max_events,
                    tool_bindings,
                    options,
                )
                .map_err(map_anthropic_response)?;
                (parsed.events, parsed.terminal)
            }
        };
        let mut converter = ProviderStreamConverter::from_gateway_receipt(receipt);
        let mut frames = Vec::new();
        for (index, event) in events.into_iter().enumerate() {
            frames.extend(converter.ingest(event).map_err(|error| {
                if matches!(
                    self.shared.config.protocol,
                    HttpsSseProviderProtocol::OpenAiChatCompletions(_)
                ) {
                    eprintln!(
                        "openai_sse_protocol stage=converter index={index} kind={:?}",
                        error.kind()
                    );
                }
                if error.kind() == crate::ProviderStreamConversionErrorKind::CredentialLeak {
                    HttpsSseProviderError::new(HttpsSseProviderErrorKind::CredentialLeak)
                } else {
                    HttpsSseProviderError::new(HttpsSseProviderErrorKind::StreamConversion)
                }
            })?);
        }
        Ok(HttpsSseProviderCompletion { frames, terminal })
    }

    fn observed_receipt(
        &self,
        bytes: &[u8],
        receipt: &ProviderGatewayOpenReceipt,
    ) -> Option<(String, ProviderTokenUsage)> {
        let observed = match self.shared.config.protocol {
            HttpsSseProviderProtocol::AnthropicMessages(options) => {
                crate::provider_anthropic::observed_anthropic_receipt(
                    bytes,
                    self.shared.config.max_event_bytes,
                    self.shared.config.max_events,
                    options,
                )
            }
            HttpsSseProviderProtocol::OpenAiChatCompletions(_) => {
                crate::provider_openai::observed_openai_usage(
                    bytes,
                    self.shared.config.max_event_bytes,
                    self.shared.config.max_events,
                )
            }
            HttpsSseProviderProtocol::CodexChatgpt | HttpsSseProviderProtocol::ChatgptPlan => {
                crate::provider_codex::observed_receipt(
                    bytes,
                    self.shared.config.max_event_bytes,
                    self.shared.config.max_events,
                )
            }
            HttpsSseProviderProtocol::OpenAiResponses { .. } => {
                crate::provider_codex::observed_api_receipt(
                    bytes,
                    self.shared.config.max_event_bytes,
                    self.shared.config.max_events,
                )
            }
            HttpsSseProviderProtocol::Canonical => None,
        };
        observed.filter(|(id, _)| {
            receipt
                .stream_leak_gate
                .inspect_bytes(crate::CredentialOutputBoundary::Persistence, id.as_bytes())
                .is_ok()
        })
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

    fn stream_response_schema(
        &self,
        receipt: &ProviderGatewayOpenReceipt,
    ) -> Result<Option<crate::provider_response_schema::ResponseSchema>, HttpsSseProviderError>
    {
        let streams = self
            .shared
            .streams
            .lock()
            .map_err(|_| HttpsSseProviderError::new(HttpsSseProviderErrorKind::Unavailable))?;
        let record = streams
            .get(&receipt.adapter_request_id)
            .filter(|record| record.model_exchange_id == receipt.model_exchange_id)
            .ok_or_else(|| {
                HttpsSseProviderError::new(HttpsSseProviderErrorKind::IdentityConflict)
            })?;
        Ok(record.response_schema.clone())
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
                return Err(
                    HttpsSseProviderError::new(HttpsSseProviderErrorKind::Transport)
                        .with_response(bytes),
                );
            }
            let read = match reader.read(&mut buffer) {
                Ok(read) => read,
                Err(error) => {
                    let mut failure =
                        HttpsSseProviderError::new(HttpsSseProviderErrorKind::Transport)
                            .with_response(bytes);
                    failure.network = Some(Box::new(
                        winwincode_network::NetworkFailure::new(
                            if error.kind() == std::io::ErrorKind::TimedOut {
                                winwincode_network::ErrorKind::Timeout
                            } else {
                                winwincode_network::ErrorKind::TransportInterrupted
                            },
                            winwincode_network::Acceptance::ResponseReceived,
                            winwincode_network::Phase::ResponseBody,
                        )
                        .with_diagnostic(winwincode_network::NetworkDiagnostic::io(&error)),
                    ));
                    return Err(failure);
                }
            };
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
            return Err(ProviderAdapterError::identity_conflict());
        }
        if matches!(
            record.state,
            StreamState::Cancelled | StreamState::Released | StreamState::Fenced
        ) {
            return Err(ProviderAdapterError::rejected());
        }
        if record.invocation_digest != *digest {
            return Err(ProviderAdapterError::identity_conflict());
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
        response_schema: Option<crate::provider_response_schema::ResponseSchema>,
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
                response_schema,
                state: StreamState::Pending,
                io: self.exchange_io(),
                metadata: ProviderFailureMetadata::default(),
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
            return Err(ProviderAdapterError::identity_conflict());
        }
        match record.state {
            StreamState::Pending => {
                record.state = state;
                Ok(())
            }
            StreamState::Cancelled | StreamState::Released | StreamState::Fenced => {
                Err(ProviderAdapterError::rejected())
            }
            StreamState::Open(_) | StreamState::Drained => {
                Err(ProviderAdapterError::identity_conflict())
            }
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
        let record = streams
            .get(invocation.adapter_request_id)
            .ok_or_else(ProviderAdapterError::identity_conflict)?;
        if record.model_exchange_id != *invocation.model_exchange_id {
            return Err(ProviderAdapterError::identity_conflict());
        }
        if matches!(
            record.state,
            StreamState::Cancelled | StreamState::Released | StreamState::Fenced
        ) {
            return Err(ProviderAdapterError::rejected());
        }
        let is_exact_pending = matches!(record.state, StreamState::Pending);
        if is_exact_pending {
            streams.remove(invocation.adapter_request_id);
            Ok(())
        } else {
            Err(ProviderAdapterError::identity_conflict())
        }
    }

    fn prepare_request(
        &self,
        invocation: HttpInvocation<'_>,
    ) -> Result<Option<PreparedAnthropicRequest>, ProviderAdapterError> {
        Ok(match self.shared.config.protocol {
            HttpsSseProviderProtocol::Canonical
            | HttpsSseProviderProtocol::OpenAiResponses { .. }
            | HttpsSseProviderProtocol::CodexChatgpt
            | HttpsSseProviderProtocol::ChatgptPlan => None,
            HttpsSseProviderProtocol::AnthropicMessages(options) => Some(
                prepare_anthropic_request(invocation.payload, invocation.model_id, options)
                    .map_err(map_anthropic_request)?,
            ),
            HttpsSseProviderProtocol::OpenAiChatCompletions(options) => Some(
                crate::provider_openai::prepare_openai_chat_request(
                    invocation.payload,
                    invocation.model_id,
                    options,
                )
                .map_err(map_anthropic_request)?,
            ),
        })
    }

    fn prepare_responses_payload(
        &self,
        invocation: HttpInvocation<'_>,
    ) -> Result<Option<Vec<u8>>, ProviderAdapterError> {
        let payload = match self.shared.config.protocol {
            HttpsSseProviderProtocol::CodexChatgpt => {
                crate::provider_codex::prepare_request(invocation.payload, invocation.model_id)
            }
            HttpsSseProviderProtocol::ChatgptPlan => {
                crate::provider_codex::prepare_plan_request(invocation.payload, invocation.model_id)
            }
            HttpsSseProviderProtocol::OpenAiResponses {
                max_output_tokens,
                structured_output,
            } => match structured_output {
                ResponsesStructuredOutput::JsonObject => {
                    crate::provider_codex::prepare_api_json_object_request(
                        invocation.payload,
                        invocation.model_id,
                        max_output_tokens,
                    )
                }
                ResponsesStructuredOutput::JsonSchema => {
                    crate::provider_codex::prepare_api_request(
                        invocation.payload,
                        invocation.model_id,
                        max_output_tokens,
                    )
                }
                ResponsesStructuredOutput::Text => crate::provider_codex::prepare_api_text_request(
                    invocation.payload,
                    invocation.model_id,
                    max_output_tokens,
                ),
            },
            _ => return Ok(None),
        };
        payload
            .map(Some)
            .map_err(|_| ProviderAdapterError::request_translation())
    }

    fn prepare_response_schema(
        &self,
        invocation: HttpInvocation<'_>,
    ) -> Result<Option<crate::provider_response_schema::ResponseSchema>, ProviderAdapterError> {
        if matches!(
            self.shared.config.protocol,
            HttpsSseProviderProtocol::OpenAiResponses {
                structured_output: ResponsesStructuredOutput::JsonObject
                    | ResponsesStructuredOutput::Text,
                ..
            }
        ) {
            crate::provider_codex::response_schema(invocation.payload)
                .map_err(|_| ProviderAdapterError::request_translation())
        } else {
            Ok(None)
        }
    }

    fn retain_failed_response(
        &self,
        mut failure: winwincode_network::NetworkFailure,
        metadata: &serde_json::Value,
        body: &[u8],
    ) -> winwincode_network::NetworkFailure {
        let directory = match self.shared.sse_failure_log.lock() {
            Ok(directory) => directory.clone(),
            Err(_) => return failure,
        };
        let Some(directory) = directory else {
            return failure;
        };
        let mut diagnostic = failure.diagnostic.unwrap_or_else(|| {
            winwincode_network::NetworkDiagnostic::new(winwincode_network::DiagnosticCode::SseEvent)
        });
        match crate::provider_sse_failure_log::retain(&directory, metadata, body) {
            Ok(id) => {
                diagnostic.response_log =
                    winwincode_network::SseResponseLogId::parse(&format!("{id}.log"));
                diagnostic.response_log_status =
                    Some(winwincode_network::ResponseLogStatus::Retained);
            }
            Err(_) => {
                diagnostic.response_log_status =
                    Some(winwincode_network::ResponseLogStatus::WriteFailed);
            }
        }
        failure.diagnostic = Some(diagnostic);
        failure
    }

    fn retain_http_error_response(
        &self,
        response: &mut ureq::http::Response<ureq::Body>,
        failure: winwincode_network::NetworkFailure,
        metadata: &ProviderFailureMetadata,
    ) -> winwincode_network::NetworkFailure {
        if !self
            .shared
            .sse_failure_log
            .lock()
            .is_ok_and(|directory| directory.is_some())
        {
            return failure;
        }
        // Capture is bounded even when successful inference streams have no total limit.
        // ExchangeIo remains in its finite opening stage; cancellation and ureq's
        // original global deadline continue to bound each read.
        let limit = self.shared.config.max_response_bytes.min(64 * 1024);
        let mut bytes = Vec::new();
        let result = response
            .body_mut()
            .as_reader()
            .take(u64::try_from(limit).unwrap_or(u64::MAX).saturating_add(1))
            .read_to_end(&mut bytes);
        let truncated = bytes.len() > limit;
        bytes.truncate(limit);
        let read_failure = result
            .as_ref()
            .err()
            .map(winwincode_network::NetworkDiagnostic::io);
        self.retain_failed_response(
            failure,
            &serde_json::json!({
                "schemaVersion": 1,
                "failure": failure,
                "providerDiagnostic": metadata.diagnostic,
                "capture": {
                    "complete": result.is_ok() && !truncated,
                    "truncated": truncated,
                    "limitBytes": limit,
                    "readFailure": read_failure,
                },
            }),
            &bytes,
        )
    }

    fn validate_http_response(
        &self,
        invocation: HttpInvocation<'_>,
        response: &mut ureq::http::Response<ureq::Body>,
        credential: &[u8],
    ) -> Result<ProviderFailureMetadata, ProviderAdapterError> {
        let status = response.status().as_u16();
        let metadata = response_metadata(
            response.headers(),
            status,
            credential,
            &self.shared.config.custom_headers,
        );
        if !(200..=299).contains(&status) {
            let failure = self.retain_http_error_response(
                response,
                winwincode_network::NetworkFailure::http(
                    status,
                    metadata
                        .provider_retry_after_millis
                        .map(Duration::from_millis),
                ),
                &metadata,
            );
            if failure.retryable() {
                self.abandon_retryable_open(invocation)?;
            } else {
                self.finish_open(invocation, StreamState::Fenced)?;
            }
            return Err(ProviderAdapterError::from_network(failure).with_metadata(metadata));
        }
        let content_type = response.headers().get("content-type");
        // These fixed official endpoints can omit MIME; the bounded Responses parser
        // still validates every event before it reaches the embedded Core.
        let responses_without_mime = content_type.is_none()
            && matches!(
                self.shared.config.protocol,
                HttpsSseProviderProtocol::CodexChatgpt | HttpsSseProviderProtocol::ChatgptPlan
            );
        if !responses_without_mime
            && !content_type
                .and_then(|value| value.to_str().ok())
                .is_some_and(canonical_event_stream_content_type)
        {
            self.abandon_retryable_open(invocation)?;
            return Err(ProviderAdapterError::response_content_type().with_metadata(metadata));
        }
        Ok(metadata)
    }

    fn http_open_failure(
        io: &winwincode_network::transport::ExchangeIo,
        error: &ureq::Error,
    ) -> winwincode_network::NetworkFailure {
        if io.is_cancelled() {
            winwincode_network::NetworkFailure::new(
                winwincode_network::ErrorKind::Cancelled,
                winwincode_network::Acceptance::Unknown,
                winwincode_network::Phase::ResponseHeaders,
            )
        } else {
            winwincode_network::classify_ureq(
                error,
                !io.connection_established(),
                winwincode_network::Phase::ResponseHeaders,
            )
        }
    }

    fn open_https(
        &self,
        invocation: HttpInvocation<'_>,
        credential: &[u8],
    ) -> Result<ProviderAdapterOpenReceipt, ProviderAdapterError> {
        let digest = invocation_digest(invocation);
        if let Some(replay) = self.existing_open(invocation, &digest)? {
            return Ok(replay);
        }
        let native_headers = native_request_headers(
            invocation.payload,
            &self.shared.config.endpoint,
            &self.shared.config.custom_headers,
        )?;
        let prepared = self.prepare_request(invocation)?;
        let tool_bindings = prepared
            .as_ref()
            .map_or_else(AnthropicToolBindings::default, |request| {
                request.tool_bindings.clone()
            });
        let responses_payload = self.prepare_responses_payload(invocation)?;
        let response_schema = self.prepare_response_schema(invocation)?;
        self.begin_open(invocation, digest, tool_bindings, response_schema)?;
        let authorization = authorization_value(credential).inspect_err(|_error| {
            let _ = self.finish_open(invocation, StreamState::Fenced);
        })?;
        let io = self
            .shared
            .streams
            .lock()
            .map_err(|_| ProviderAdapterError::unavailable())?
            .get(invocation.adapter_request_id)
            .map(|record| Arc::clone(&record.io))
            .ok_or_else(ProviderAdapterError::unavailable)?;
        let agent = Self::agent(&self.shared.config, Arc::clone(&io));
        let mut request = agent
            .post(&self.shared.config.endpoint)
            .header("Accept", "text/event-stream")
            .header("Authorization", &authorization)
            .header("Idempotency-Key", invocation.adapter_request_id)
            .header("X-WinWinCode-Model", invocation.model_id);
        for (name, value) in &native_headers {
            request = request.header(*name, value.as_str());
        }
        for (name, value) in &self.shared.config.custom_headers {
            request = request.header(name.as_str(), value.as_str());
        }
        if matches!(
            self.shared.config.protocol,
            HttpsSseProviderProtocol::CodexChatgpt
        ) {
            let account = self
                .shared
                .config
                .codex_account_id
                .as_deref()
                .ok_or_else(ProviderAdapterError::rejected)?;
            request = request
                .header("ChatGPT-Account-Id", account)
                .header("OpenAI-Beta", "responses=experimental")
                .header("originator", "codex_cli_rs");
        }
        let payload = if let Some(payload) = responses_payload.as_ref() {
            request = request.header("Content-Type", "application/json");
            payload.as_slice()
        } else if let Some(prepared) = prepared.as_ref() {
            request = request.header("Content-Type", "application/json");
            match self.shared.config.protocol {
                HttpsSseProviderProtocol::AnthropicMessages(_) => {
                    request = request.header("Anthropic-Version", "2023-06-01");
                }
                HttpsSseProviderProtocol::Canonical
                | HttpsSseProviderProtocol::OpenAiResponses { .. }
                | HttpsSseProviderProtocol::CodexChatgpt
                | HttpsSseProviderProtocol::ChatgptPlan
                | HttpsSseProviderProtocol::OpenAiChatCompletions(_) => {}
            }
            prepared.body.as_slice()
        } else {
            request = request.header("Content-Type", invocation.content_type);
            invocation.payload
        };
        let response = request.send(payload);
        drop(authorization);
        let mut response = match response {
            Ok(response) => response,
            Err(error) => {
                self.abandon_retryable_open(invocation)?;
                return Err(ProviderAdapterError::from_network(Self::http_open_failure(
                    &io, &error,
                )));
            }
        };
        let metadata = self.validate_http_response(invocation, &mut response, credential)?;
        self.shared
            .streams
            .lock()
            .map_err(|_| ProviderAdapterError::unavailable())?
            .get_mut(invocation.adapter_request_id)
            .ok_or_else(ProviderAdapterError::unavailable)?
            .metadata = metadata;
        io.body_started();
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

    fn open_recovering(
        &self,
        invocation: &ProviderAdapterInvocation<'_>,
        credential: &ResolvedSecret,
        receipt: &ProviderGatewayOpenReceipt,
        can_start: &(dyn Fn() -> bool + Sync),
    ) -> Result<ProviderAdapterOpenReceipt, ProviderAdapterError> {
        self.open_recovering_admitted(invocation, credential, receipt, can_start, || Ok(()))
    }

    fn control(
        &self,
        model_exchange_id: &ModelExchangeId,
        adapter_request_id: &str,
        action: ProviderStreamControlAction,
    ) -> Result<(), ProviderAdapterError> {
        if !valid_token(adapter_request_id, MAX_ADAPTER_REQUEST_ID_BYTES) {
            return Err(ProviderAdapterError::identity_conflict());
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
                response_schema: None,
                state: StreamState::Fenced,
                io: self.exchange_io(),
                metadata: ProviderFailureMetadata::default(),
            });
        if record.model_exchange_id != *model_exchange_id {
            return Err(ProviderAdapterError::identity_conflict());
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
                record.io.cancel();
                record.state = StreamState::Cancelled;
                record.controls |= CONTROL_CANCEL;
            }
            ProviderStreamControlAction::Release => {
                record.io.cancel();
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
    let frames = crate::provider_sse_framing::parse(bytes, max_event_bytes, max_events).map_err(
        |error| {
            HttpsSseProviderError::new(match error {
                crate::provider_sse_framing::SseFramingError::Utf8 => {
                    HttpsSseProviderErrorKind::SseFraming
                }
                crate::provider_sse_framing::SseFramingError::SizeLimit => {
                    HttpsSseProviderErrorKind::SizeLimit
                }
            })
        },
    )?;
    let mut wire_events = Vec::new();
    for frame in frames {
        if frame
            .event
            .as_deref()
            .is_some_and(|name| !name.is_empty() && name != "message")
        {
            return Err(HttpsSseProviderError::new(
                HttpsSseProviderErrorKind::SseEvent,
            ));
        }
        let mut data = frame.data;
        dispatch_event(&mut wire_events, &mut data, max_event_bytes, max_events)?;
    }
    canonical_events(wire_events)
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
        .map_err(|_| HttpsSseProviderError::new(HttpsSseProviderErrorKind::SseEvent))?;
    events.push(event);
    data.clear();
    Ok(())
}

fn canonical_events(wire: Vec<ProviderWireEvent>) -> Result<ParsedStream, HttpsSseProviderError> {
    let mut canonical = CanonicalEvents::new(wire.len());
    for event in wire {
        if canonical.terminal.is_some() {
            return Err(HttpsSseProviderError::new(
                HttpsSseProviderErrorKind::SseEvent,
            ));
        }
        canonical.push(event)?;
    }
    canonical.finish()
}

struct CanonicalEvents {
    events: Vec<ProviderStreamEvent>,
    usage: Option<ProviderTokenUsage>,
    // Outer None means no cost meter; inner None means an explicit unknown cost.
    #[allow(clippy::option_option)]
    cost_micros: Option<Option<u64>>,
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
            ProviderWireEvent::ResponseStarted {
                response_id,
                model_id,
            } => ProviderStreamEvent::ResponseStarted {
                provider_response_id: response_id,
                observed_model_id: model_id,
            },
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
                    .map_err(|_| HttpsSseProviderError::new(HttpsSseProviderErrorKind::SseEvent))?,
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
                    HttpsSseProviderErrorKind::SseEvent,
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
                    || actual_cost_micros.is_some_and(|cost| cost > MAX_SAFE_INTEGER)
                {
                    return Err(HttpsSseProviderError::new(
                        HttpsSseProviderErrorKind::SseEvent,
                    ));
                }
                self.events.push(ProviderStreamEvent::Usage(value));
            }
            ProviderWireEvent::Finished { reason } => {
                let usage = self.usage.ok_or_else(|| {
                    HttpsSseProviderError::new(HttpsSseProviderErrorKind::SseEvent)
                })?;
                let cost = self.cost_micros.ok_or_else(|| {
                    HttpsSseProviderError::new(HttpsSseProviderErrorKind::SseEvent)
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
                    HttpsSseProviderErrorKind::SseEvent,
                ));
            }
        }
        Ok(())
    }

    fn finish(self) -> Result<ParsedStream, HttpsSseProviderError> {
        let terminal = self.terminal.ok_or_else(|| {
            HttpsSseProviderError::new(HttpsSseProviderErrorKind::IncompleteStream)
        })?;
        Ok(ParsedStream {
            events: self.events,
            terminal,
        })
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
    ResponseStarted {
        response_id: String,
        model_id: Option<String>,
    },
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
        cached_input_tokens: Option<u64>,
        cache_write_input_tokens: u64,
        output_tokens: u64,
        reasoning_output_tokens: u64,
        actual_cost_micros: Option<u64>,
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

pub(crate) fn validate_custom_headers(
    headers: &[(String, String)],
) -> Result<(), HttpsSseProviderError> {
    if headers.len() > MAX_CUSTOM_HEADERS {
        return Err(HttpsSseProviderError::new(
            HttpsSseProviderErrorKind::InvalidConfiguration,
        ));
    }
    let mut names = std::collections::BTreeSet::new();
    for (name, value) in headers {
        if !names.insert(name.to_ascii_lowercase())
            || name.is_empty()
            || name.len() > MAX_CUSTOM_HEADER_NAME_BYTES
            || name.trim() != name
            || !name
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || "!#$%&'*+.^_`|~-".contains(c))
            || FORBIDDEN_CUSTOM_HEADERS.contains(&name.to_ascii_lowercase().as_str())
            || value.is_empty()
            || value.len() > MAX_CUSTOM_HEADER_VALUE_BYTES
            || value.chars().any(char::is_control)
        {
            return Err(HttpsSseProviderError::new(
                HttpsSseProviderErrorKind::InvalidConfiguration,
            ));
        }
    }
    Ok(())
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
        AnthropicCodecErrorKind::InvalidRequest => ProviderAdapterError::request_invalid(),
        AnthropicCodecErrorKind::SizeLimit => ProviderAdapterError::request_size_limit(),
        AnthropicCodecErrorKind::Protocol
        | AnthropicCodecErrorKind::InvalidSse
        | AnthropicCodecErrorKind::IncompleteStream => ProviderAdapterError::request_translation(),
    }
}

fn map_anthropic_response(
    error: crate::provider_anthropic::AnthropicCodecError,
) -> HttpsSseProviderError {
    let mut mapped = HttpsSseProviderError::new(match error.kind() {
        AnthropicCodecErrorKind::InvalidRequest | AnthropicCodecErrorKind::Protocol => {
            HttpsSseProviderErrorKind::SseEvent
        }
        AnthropicCodecErrorKind::InvalidSse => HttpsSseProviderErrorKind::SseFraming,
        AnthropicCodecErrorKind::IncompleteStream => HttpsSseProviderErrorKind::IncompleteStream,
        AnthropicCodecErrorKind::SizeLimit => HttpsSseProviderErrorKind::SizeLimit,
    });
    mapped.metadata.diagnostic = error
        .diagnostic()
        .map(|diagnostic| ProviderFailureDiagnostic {
            stage: diagnostic.stage.to_owned(),
            event_type: diagnostic.event_type.to_owned(),
            field_path: diagnostic.field_path.to_owned(),
        });
    mapped
}

fn response_metadata(
    headers: &ureq::http::HeaderMap,
    status: u16,
    credential: &[u8],
    custom_headers: &[(String, String)],
) -> ProviderFailureMetadata {
    let provider_retry_after_millis = headers
        .get("retry-after")
        .and_then(|value| value.to_str().ok())
        .and_then(|value| {
            let value = value.trim();
            value
                .parse::<u64>()
                .ok()
                .and_then(|seconds| seconds.checked_mul(1_000))
                .or_else(|| {
                    httpdate::parse_http_date(value).ok().map(|deadline| {
                        u64::try_from(
                            deadline
                                .duration_since(std::time::SystemTime::now())
                                .unwrap_or(Duration::ZERO)
                                .as_millis(),
                        )
                        .unwrap_or(u64::MAX)
                    })
                })
        });
    let provider_request_id = ["x-request-id", "request-id", "x-amzn-requestid"]
        .iter()
        .find_map(|name| headers.get(*name).and_then(|value| value.to_str().ok()))
        .filter(|value| {
            !value.is_empty()
                && value.len() <= 256
                && value.bytes().all(|byte| byte.is_ascii_graphic())
        })
        .filter(|value| {
            !value
                .as_bytes()
                .windows(credential.len().max(1))
                .any(|window| window == credential)
                && !custom_headers
                    .iter()
                    .any(|(_, secret)| !secret.is_empty() && value.contains(secret))
        })
        .map(str::to_owned);
    ProviderFailureMetadata {
        status: Some(status),
        provider_retry_after_millis,
        provider_request_id,
        diagnostic: None,
    }
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

#[cfg(test)]
fn classify_open_error(error: &ureq::Error) -> ProviderAdapterError {
    ProviderAdapterError::from_network(winwincode_network::classify_ureq(
        error,
        false,
        winwincode_network::Phase::ResponseHeaders,
    ))
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

/// Derive routing headers from the kernel's original request envelope before
/// protocol translation removes it. Never mutate shared Provider configuration:
/// concurrent conversations and physical retries retain their own identities.
fn native_request_headers(
    payload: &[u8],
    endpoint: &str,
    custom_headers: &[(String, String)],
) -> Result<Vec<(&'static str, String)>, ProviderAdapterError> {
    let mut headers = Vec::with_capacity(4);
    let mut append = |name: &'static str, value: &str| {
        if !custom_headers
            .iter()
            .any(|(configured, _)| configured.eq_ignore_ascii_case(name))
        {
            headers.push((name, value.to_owned()));
        }
    };
    append(
        "User-Agent",
        concat!("WinWinCode/", env!("CARGO_PKG_VERSION")),
    );
    // Raw canonical payloads are also supported; they have no Core envelope.
    let Ok(value) = serde_json::from_slice::<serde_json::Value>(payload) else {
        return Ok(headers);
    };
    let Some(envelope) = value.as_object() else {
        return Ok(headers);
    };
    if !["requestId", "provider", "request"]
        .iter()
        .all(|field| envelope.contains_key(*field))
    {
        return Ok(headers);
    }
    let identity = |field| {
        envelope
            .get(field)
            .and_then(serde_json::Value::as_str)
            .filter(|id| {
                !id.is_empty()
                    && id.len() <= 512
                    && id.bytes().all(|byte| (0x21..=0x7e).contains(&byte))
            })
            .ok_or_else(ProviderAdapterError::request_translation)
    };
    let session_id = identity("sessionId")?;
    let thread_id = identity("threadId")?;
    append("session-id", session_id);
    append("thread-id", thread_id);
    // OpenCode Go/Zen expects an explicit per-conversation routing alias from
    // clients with their own User-Agent. Restrict it to the official HTTPS origin.
    let opencode_origin = ureq::http::Uri::from_str(endpoint).is_ok_and(|uri| {
        uri.scheme_str() == Some("https")
            && uri.authority().is_some_and(|authority| {
                authority.as_str().eq_ignore_ascii_case("opencode.ai")
                    || authority.as_str().eq_ignore_ascii_case("opencode.ai:443")
            })
    });
    if opencode_origin {
        append("x-opencode-session", thread_id);
    }
    Ok(headers)
}

/// Canonical HTTPS endpoint shape for every Provider module: a trimmed
/// `https` URI with a host, no embedded userinfo, and no query or fragment.
/// The fragment guard is explicit because `Uri::from_str` strips a fragment
/// instead of rejecting it. Other Provider modules validate through this
/// check, so an endpoint can never be accepted at preset time and rejected at
/// request time, or the reverse.
pub fn canonical_https_endpoint(value: &str) -> bool {
    if value.trim() != value || value.contains('#') {
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
    // SSE is always decoded as UTF-8; MIME parameters do not alter that.
    value
        .split(';')
        .next()
        .is_some_and(|essence| essence.trim().eq_ignore_ascii_case("text/event-stream"))
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
        net::{Shutdown, TcpListener, TcpStream},
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
    fn only_transient_connection_failures_request_another_attempt() {
        for error in [
            ureq::Error::HostNotFound,
            ureq::Error::ConnectionFailed,
            ureq::Error::Timeout(ureq::Timeout::Connect),
            ureq::Error::Io(std::io::Error::from(std::io::ErrorKind::ConnectionReset)),
            ureq::Error::Io(std::io::Error::from(std::io::ErrorKind::UnexpectedEof)),
        ] {
            assert!(classify_open_error(&error).retryable());
        }
        for error in [
            ureq::Error::Tls("private TLS diagnostic"),
            ureq::Error::TlsRequired,
            ureq::Error::InvalidProxyUrl,
            ureq::Error::BadUri("private request".to_owned()),
            ureq::Error::TooManyRedirects,
            ureq::Error::Rustls(rustls::Error::InvalidCertificate(
                rustls::CertificateError::UnknownIssuer,
            )),
            ureq::Error::Io(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                rustls::Error::InvalidCertificate(rustls::CertificateError::NotValidForName),
            )),
        ] {
            let failure = classify_open_error(&error);
            assert!(
                !failure.retryable(),
                "error category {:?}, safe failure {failure:?}",
                std::mem::discriminant(&error)
            );
            assert!(!format!("{failure:?}").contains("private"));
        }
    }

    const SECRET: &[u8] = b"provider-https-sse-secret-fixture";
    const PAYLOAD: &[u8] = br#"{"input":"local TLS fixture"}"#;

    include!("provider_http_error_diagnostics.test.rs");
    include!("provider_request_identity_diagnostics.test.rs");

    fn responses_api_payload() -> Vec<u8> {
        serde_json::to_vec(&serde_json::json!({"request":{
            "model":"fixture-model", "instructions":"Use native tools", "stream":true, "store":false,
            "tools":[{"type":"custom","name":"exec","description":"Raw JavaScript","format":{"type":"grammar","syntax":"lark","definition":"start: /[\\s\\S]+/"}}],
            "input":[{"type":"custom_tool_call","name":"exec","call_id":"old-call","input":"text('previous');"},{"type":"custom_tool_call_output","call_id":"old-call","output":"previous"}]
        }})).unwrap()
    }

    fn responses_api_body(source: &str) -> String {
        use std::fmt::Write as _;

        let events = [
            serde_json::json!({"type":"response.created","response":{"id":"resp-api","model":"observed-api-model"}}),
            serde_json::json!({"type":"response.output_item.added","item":{"type":"custom_tool_call","id":"ct-api","call_id":"call-api","name":"exec","input":""}}),
            serde_json::json!({"type":"response.custom_tool_call_input.delta","item_id":"ct-api","call_id":"call-api","delta":source}),
            serde_json::json!({"type":"response.output_item.done","item":{"type":"custom_tool_call","id":"ct-api","call_id":"call-api","name":"exec","input":source}}),
            serde_json::json!({"type":"response.completed","response":{"id":"resp-api","model":"observed-api-model","usage":{"input_tokens":20,"output_tokens":7},"end_turn":true}}),
        ];
        let mut body = String::new();
        for event in events {
            write!(&mut body, "data: {event}\n\n").unwrap();
        }
        body
    }

    fn responses_api_receipt(
        exchange: ModelExchangeId,
        request_id: RequestId,
    ) -> ProviderGatewayOpenReceipt {
        let mut stream_leak_gate = crate::CredentialLeakGate::new();
        stream_leak_gate.track_secret(&ResolvedSecret::from_bytes(SECRET.to_vec()).unwrap());
        ProviderGatewayOpenReceipt {
            model_exchange_id: exchange,
            request_id,
            route: winwincode_api::generated::ModelRoute {
                provider_id: "provider-https-fixture".into(),
                model_id: "fixture-model".into(),
                credential_reference_id: winwincode_domain::CredentialReferenceId(
                    "crd_00000000000000000000000001".into(),
                ),
            },
            adapter_request_id: "pad_00000000000000000000000001".into(),
            idempotent_replay: false,
            stream_leak_gate,
        }
    }

    fn json_object_payload() -> Vec<u8> {
        let mut payload: serde_json::Value =
            serde_json::from_slice(&responses_api_payload()).unwrap();
        payload["request"]["text"] = serde_json::json!({"format":{
            "type":"json_schema","strict":true,"name":"fixture_result",
            "schema":{"type":"object","additionalProperties":false,
                "required":["ok","findings"],"properties":{
                    "ok":{"type":"boolean"},
                    "findings":{"type":"array","items":{"type":"object",
                        "additionalProperties":false,"required":["verdict"],
                        "properties":{"verdict":{"type":"string","enum":["pass","fail"]}}}}
                }}
        }});
        serde_json::to_vec(&payload).unwrap()
    }

    fn json_object_body(answer: &str) -> String {
        use std::fmt::Write as _;

        let mut events = Vec::new();
        events.push(serde_json::json!({"type":"response.created","response":{"id":"resp-api","model":"observed-api-model"}}));
        events.push(serde_json::json!({"type":"response.output_item.added","item":{"type":"message","id":"message-api","role":"assistant","phase":"final_answer","content":[]}}));
        events.push(serde_json::json!({"type":"response.output_item.done","item":{"type":"message","id":"message-api","role":"assistant","phase":"final_answer","content":[{"type":"output_text","text":answer}]}}));
        events.push(serde_json::json!({"type":"response.completed","response":{"id":"resp-api","model":"observed-api-model","usage":{"input_tokens":20,"output_tokens":7},"end_turn":true}}));
        let mut body = String::new();
        for event in events {
            writeln!(&mut body, "data: {event}\n").unwrap();
        }
        body
    }

    #[test]
    fn json_object_responses_https_maps_only_format_and_keeps_canonical_schema() {
        let payload = json_object_payload();
        let original: serde_json::Value = serde_json::from_slice(&payload).unwrap();
        let body = json_object_body(r#"{"ok":true,"findings":[{"verdict":"pass"}]}"#);
        let fixture = TlsFixture::start(vec![TestResponse {
            status: "200 OK",
            content_type: "text/event-stream",
            body: &body,
            declared_length: None,
            delay: Duration::ZERO,
        }]);
        let adapter = HttpsSseProviderAdapter::try_new(
            config(&fixture)
                .with_openai_responses_json_object(32_768)
                .unwrap(),
        )
        .unwrap();
        let exchange = ModelExchangeId("mdl_00000000000000000000000001".into());
        let request_id = RequestId("req_00000000000000000000000001".into());
        let mut request = invocation(&exchange, &request_id);
        request.payload = &payload;
        adapter.open_https(request, SECRET).unwrap();
        let completion = adapter
            .drain_canonical(&responses_api_receipt(exchange, request_id))
            .unwrap();
        assert!(matches!(
            completion.terminal,
            ProviderGatewayTerminal::Completed { .. }
        ));
        let requests = fixture.finish();
        let start = find_bytes(&requests[0], b"\r\n\r\n").unwrap() + 4;
        let sent: serde_json::Value = serde_json::from_slice(&requests[0][start..]).unwrap();
        assert_eq!(
            sent["text"]["format"],
            serde_json::json!({"type":"json_object"})
        );
        assert_eq!(sent["tools"], original["request"]["tools"]);
        assert_eq!(sent["input"], original["request"]["input"]);
        let instructions = sent["instructions"].as_str().unwrap();
        assert!(instructions.starts_with(original["request"]["instructions"].as_str().unwrap()));
        assert!(instructions.contains(
            &serde_json::to_string(&original["request"]["text"]["format"]["schema"]).unwrap()
        ));
        assert_eq!(
            serde_json::from_slice::<serde_json::Value>(&payload).unwrap(),
            original
        );
    }

    #[test]
    fn text_responses_https_omits_text_and_retains_native_tools_and_schema() {
        let payload = json_object_payload();
        let original: serde_json::Value = serde_json::from_slice(&payload).unwrap();
        let body = json_object_body(r#"{"ok":true,"findings":[{"verdict":"pass"}]}"#);
        let fixture = TlsFixture::start(vec![TestResponse {
            status: "200 OK",
            content_type: "text/event-stream",
            body: &body,
            declared_length: None,
            delay: Duration::ZERO,
        }]);
        let adapter = HttpsSseProviderAdapter::try_new(
            config(&fixture).with_openai_responses_text(32_768).unwrap(),
        )
        .unwrap();
        let exchange = ModelExchangeId("mdl_00000000000000000000000001".into());
        let request_id = RequestId("req_00000000000000000000000001".into());
        let mut request = invocation(&exchange, &request_id);
        request.payload = &payload;
        adapter.open_https(request, SECRET).unwrap();
        let completion = adapter
            .drain_canonical(&responses_api_receipt(exchange, request_id))
            .unwrap();
        assert!(matches!(
            completion.terminal,
            ProviderGatewayTerminal::Completed { .. }
        ));
        let requests = fixture.finish();
        let start = find_bytes(&requests[0], b"\r\n\r\n").unwrap() + 4;
        let sent: serde_json::Value = serde_json::from_slice(&requests[0][start..]).unwrap();
        assert!(
            sent.get("text").is_none(),
            "Text mode uses native default format"
        );
        assert_eq!(sent["tools"], original["request"]["tools"]);
        assert_eq!(sent["input"], original["request"]["input"]);
        let instructions = sent["instructions"].as_str().unwrap();
        assert!(instructions.starts_with(original["request"]["instructions"].as_str().unwrap()));
        assert_eq!(
            instructions
                .matches(
                    &serde_json::to_string(&original["request"]["text"]["format"]["schema"])
                        .unwrap()
                )
                .count(),
            1
        );
        assert_eq!(
            serde_json::from_slice::<serde_json::Value>(&payload).unwrap(),
            original
        );
    }

    #[test]
    fn text_responses_https_rejects_invalid_final_with_paid_usage_and_real_model() {
        let payload = json_object_payload();
        let body = json_object_body(r#"{"ok":"wrong","findings":[]}"#);
        let fixture = TlsFixture::start(vec![TestResponse {
            status: "200 OK",
            content_type: "text/event-stream",
            body: &body,
            declared_length: None,
            delay: Duration::ZERO,
        }]);
        let adapter = HttpsSseProviderAdapter::try_new(
            config(&fixture).with_openai_responses_text(32_768).unwrap(),
        )
        .unwrap();
        let exchange = ModelExchangeId("mdl_00000000000000000000000001".into());
        let request_id = RequestId("req_00000000000000000000000001".into());
        let mut request = invocation(&exchange, &request_id);
        request.payload = &payload;
        adapter.open_https(request, SECRET).unwrap();
        let completion = adapter
            .drain_canonical(&responses_api_receipt(exchange, request_id))
            .unwrap();
        assert!(
            matches!(completion.terminal, ProviderGatewayTerminal::Failed { charge: Some(charge), .. } if charge.usage.input_tokens == 20 && charge.usage.output_tokens == 7)
        );
        let frames = completion
            .frames
            .iter()
            .map(|frame| serde_json::from_str::<serde_json::Value>(frame.payload_json()).unwrap())
            .collect::<Vec<_>>();
        assert!(
            frames
                .iter()
                .any(|frame| frame["type"] == "server_model"
                    && frame["model"] == "observed-api-model")
        );
        assert!(!frames.iter().any(|frame| frame["type"] == "completed"));
        assert_eq!(
            frames.last().unwrap()["error"]["code"],
            "RESPONSE_SCHEMA_INVALID"
        );
        assert_eq!(frames.last().unwrap()["error"]["retryable"], false);
        assert_eq!(fixture.finish().len(), 1);
    }

    #[test]
    fn text_responses_https_accepts_native_tool_input_with_a_final_schema() {
        let payload = json_object_payload();
        let source = "const result = await tools.exec_command({cmd:'printf ready'}); text(result);";
        let body = responses_api_body(source);
        let fixture = TlsFixture::start(vec![TestResponse {
            status: "200 OK",
            content_type: "text/event-stream",
            body: &body,
            declared_length: None,
            delay: Duration::ZERO,
        }]);
        let adapter = HttpsSseProviderAdapter::try_new(
            config(&fixture).with_openai_responses_text(32_768).unwrap(),
        )
        .unwrap();
        let exchange = ModelExchangeId("mdl_00000000000000000000000001".into());
        let request_id = RequestId("req_00000000000000000000000001".into());
        let mut request = invocation(&exchange, &request_id);
        request.payload = &payload;
        adapter.open_https(request, SECRET).unwrap();
        let completion = adapter
            .drain_canonical(&responses_api_receipt(exchange, request_id))
            .unwrap();
        assert!(matches!(
            completion.terminal,
            ProviderGatewayTerminal::Completed { .. }
        ));
        let frames = completion
            .frames
            .iter()
            .map(|frame| serde_json::from_str::<serde_json::Value>(frame.payload_json()).unwrap())
            .collect::<Vec<_>>();
        let item = &frames
            .iter()
            .find(|frame| frame["type"] == "output_item_done")
            .unwrap()["item"];
        assert_eq!(item["type"], "custom_tool_call");
        assert_eq!(item["id"], "ct-api");
        assert_eq!(item["call_id"], "call-api");
        assert_eq!(item["input"], source);
        assert_eq!(fixture.finish().len(), 1);
    }

    #[test]
    fn text_responses_device_config_selects_explicit_wire_mode_and_keeps_defaults() {
        let payload = json_object_payload();
        let original: serde_json::Value = serde_json::from_slice(&payload).unwrap();
        for mode in [None, Some("json_schema"), Some("json_object"), Some("text")] {
            let mut config = serde_json::json!({
                "providerId":"provider-https-fixture","displayName":"Public fixture",
                "endpoint":"https://models.example/v1/responses","protocol":"openai_responses",
                "modelIds":["fixture-model"],"enabled":true
            });
            if let Some(mode) = mode {
                config["responsesStructuredOutput"] = mode.into();
            }
            let config = serde_json::from_value(config).unwrap();
            let adapter = crate::device_store::adapter(&config, BTreeMap::new(), None).unwrap();
            let exchange = ModelExchangeId("mdl_00000000000000000000000001".into());
            let request_id = RequestId("req_00000000000000000000000001".into());
            let mut request = invocation(&exchange, &request_id);
            request.payload = &payload;
            let bytes = adapter.prepare_responses_payload(request).unwrap().unwrap();
            let sent: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
            assert_eq!(sent["tools"], original["request"]["tools"]);
            assert_eq!(sent["input"], original["request"]["input"]);
            match mode {
                Some("text") => assert!(sent.get("text").is_none()),
                Some("json_object") => assert_eq!(
                    sent["text"]["format"],
                    serde_json::json!({"type":"json_object"})
                ),
                None | Some("json_schema") => {
                    assert_eq!(sent["text"], original["request"]["text"]);
                    assert_eq!(sent["instructions"], original["request"]["instructions"]);
                }
                _ => unreachable!(),
            }
        }
    }

    #[test]
    fn text_responses_rejects_unknown_schema_before_network() {
        let fixture = TlsFixture::start(Vec::new());
        let adapter = HttpsSseProviderAdapter::try_new(
            config(&fixture).with_openai_responses_text(32_768).unwrap(),
        )
        .unwrap();
        let mut payload: serde_json::Value =
            serde_json::from_slice(&json_object_payload()).unwrap();
        payload["request"]["text"]["format"]["schema"]["properties"]["ok"]["pattern"] =
            serde_json::json!("unsupported");
        let payload = serde_json::to_vec(&payload).unwrap();
        let exchange = ModelExchangeId("mdl_00000000000000000000000001".into());
        let request_id = RequestId("req_00000000000000000000000001".into());
        let mut request = invocation(&exchange, &request_id);
        request.payload = &payload;
        assert!(adapter.open_https(request, SECRET).is_err());
        assert_eq!(fixture.finish().len(), 0);
    }

    #[test]
    fn json_object_responses_https_rejects_invalid_final_retaining_usage_and_model() {
        let payload = json_object_payload();
        let body = json_object_body(r#"{"ok":"wrong","findings":[]}"#);
        let fixture = TlsFixture::start(vec![TestResponse {
            status: "200 OK",
            content_type: "text/event-stream",
            body: &body,
            declared_length: None,
            delay: Duration::ZERO,
        }]);
        let adapter = HttpsSseProviderAdapter::try_new(
            config(&fixture)
                .with_openai_responses_json_object(32_768)
                .unwrap(),
        )
        .unwrap();
        let exchange = ModelExchangeId("mdl_00000000000000000000000001".into());
        let request_id = RequestId("req_00000000000000000000000001".into());
        let mut request = invocation(&exchange, &request_id);
        request.payload = &payload;
        adapter.open_https(request, SECRET).unwrap();
        let completion = adapter
            .drain_canonical(&responses_api_receipt(exchange, request_id))
            .unwrap();
        assert!(
            matches!(completion.terminal, ProviderGatewayTerminal::Failed { charge: Some(charge), .. } if charge.usage.input_tokens == 20 && charge.usage.output_tokens == 7)
        );
        let frames = completion
            .frames
            .iter()
            .map(|frame| serde_json::from_str::<serde_json::Value>(frame.payload_json()).unwrap())
            .collect::<Vec<_>>();
        assert!(
            frames
                .iter()
                .any(|frame| frame["type"] == "server_model"
                    && frame["model"] == "observed-api-model")
        );
        assert!(!frames.iter().any(|frame| frame["type"] == "completed"));
        assert_eq!(frames.last().unwrap()["type"], "error");
        assert_eq!(fixture.finish().len(), 1);
    }

    #[test]
    fn json_object_responses_rejects_unknown_schema_before_network() {
        let fixture = TlsFixture::start(Vec::new());
        let adapter = HttpsSseProviderAdapter::try_new(
            config(&fixture)
                .with_openai_responses_json_object(32_768)
                .unwrap(),
        )
        .unwrap();
        let mut payload: serde_json::Value =
            serde_json::from_slice(&json_object_payload()).unwrap();
        payload["request"]["text"]["format"]["schema"]["properties"]["ok"]["pattern"] =
            serde_json::json!("unsupported");
        let payload = serde_json::to_vec(&payload).unwrap();
        let exchange = ModelExchangeId("mdl_00000000000000000000000001".into());
        let request_id = RequestId("req_00000000000000000000000001".into());
        let mut request = invocation(&exchange, &request_id);
        request.payload = &payload;
        assert!(adapter.prepare_responses_payload(request).is_err());
        assert_eq!(fixture.finish().len(), 0);
    }

    #[test]
    fn openai_responses_https_preserves_custom_contract_and_observes_real_model() {
        let payload = responses_api_payload();
        let original: serde_json::Value = serde_json::from_slice(&payload).unwrap();
        let source = "text(true);\ntext('中文');";
        let body = responses_api_body(source);
        let fixture = TlsFixture::start(vec![TestResponse {
            status: "200 OK",
            content_type: "text/event-stream",
            body: &body,
            declared_length: None,
            delay: Duration::ZERO,
        }]);
        let settings = config(&fixture)
            .with_openai_responses(32_768)
            .unwrap()
            .with_custom_headers([(
                "x-openai-internal-codex-responses-lite".into(),
                "true".into(),
            )])
            .unwrap();
        let adapter = HttpsSseProviderAdapter::try_new(settings).unwrap();
        let exchange = ModelExchangeId("mdl_00000000000000000000000001".into());
        let request_id = RequestId("req_00000000000000000000000001".into());
        let mut request = invocation(&exchange, &request_id);
        request.payload = &payload;
        adapter.open_https(request, SECRET).unwrap();
        let completion = adapter
            .drain_canonical(&responses_api_receipt(exchange, request_id))
            .unwrap();
        let frames = completion
            .frames
            .iter()
            .map(|frame| serde_json::from_str::<serde_json::Value>(frame.payload_json()).unwrap())
            .collect::<Vec<_>>();
        assert!(
            frames
                .iter()
                .any(|frame| frame["type"] == "server_model"
                    && frame["model"] == "observed-api-model")
        );
        assert!(
            frames
                .iter()
                .any(|frame| frame["type"] == "output_item_done"
                    && frame["item"]["type"] == "custom_tool_call"
                    && frame["item"]["input"] == source)
        );
        let requests = fixture.finish();
        assert_eq!(requests.len(), 1);
        let request = &requests[0];
        assert!(contains_ascii_case_insensitive(
            request,
            b"x-openai-internal-codex-responses-lite: true"
        ));
        assert!(contains_ascii_case_insensitive(
            request,
            b"Authorization: Bearer provider-https-sse-secret-fixture"
        ));
        for header in [
            b"ChatGPT-Account-Id:".as_slice(),
            b"OpenAI-Beta:",
            b"Anthropic-Version:",
            b"originator:",
        ] {
            assert!(!contains_ascii_case_insensitive(request, header));
        }
        let start = find_bytes(request, b"\r\n\r\n").unwrap() + 4;
        let sent: serde_json::Value = serde_json::from_slice(&request[start..]).unwrap();
        assert_eq!(sent["tools"], original["request"]["tools"]);
        assert_eq!(sent["input"], original["request"]["input"]);
        assert_eq!(sent["max_output_tokens"], 32_768);
    }

    #[test]
    fn openai_responses_https_still_blocks_keys_and_private_header_echoes() {
        for echoed in ["private-header-token", std::str::from_utf8(SECRET).unwrap()] {
            let body = responses_api_body(&format!("text('{echoed}');"));
            let fixture = TlsFixture::start(vec![TestResponse {
                status: "200 OK",
                content_type: "text/event-stream",
                body: &body,
                declared_length: None,
                delay: Duration::ZERO,
            }]);
            let settings = config(&fixture)
                .with_openai_responses(32_768)
                .unwrap()
                .with_custom_headers([
                    (
                        "x-openai-internal-codex-responses-lite".into(),
                        "true".into(),
                    ),
                    (
                        "x-private-provider-header".into(),
                        "private-header-token".into(),
                    ),
                ])
                .unwrap();
            let adapter = HttpsSseProviderAdapter::try_new(settings).unwrap();
            let exchange = ModelExchangeId("mdl_00000000000000000000000001".into());
            let request_id = RequestId("req_00000000000000000000000001".into());
            let payload = responses_api_payload();
            let mut request = invocation(&exchange, &request_id);
            request.payload = &payload;
            adapter.open_https(request, SECRET).unwrap();
            assert_eq!(
                adapter
                    .drain_canonical(&responses_api_receipt(exchange, request_id))
                    .unwrap_err()
                    .kind(),
                HttpsSseProviderErrorKind::CredentialLeak
            );
            assert_eq!(fixture.finish().len(), 1);
        }
    }

    #[test]
    fn openai_responses_https_requires_sse_mime() {
        for content_type in ["", "application/json"] {
            let body = responses_api_body("text(true);");
            let fixture = TlsFixture::start(vec![TestResponse {
                status: "200 OK",
                content_type,
                body: &body,
                declared_length: None,
                delay: Duration::ZERO,
            }]);
            let adapter = HttpsSseProviderAdapter::try_new(
                config(&fixture).with_openai_responses(32_768).unwrap(),
            )
            .unwrap();
            let exchange = ModelExchangeId("mdl_00000000000000000000000001".into());
            let request_id = RequestId("req_00000000000000000000000001".into());
            let payload = responses_api_payload();
            let mut request = invocation(&exchange, &request_id);
            request.payload = &payload;
            assert_eq!(
                format!(
                    "{:?}",
                    adapter.open_https(request, SECRET).unwrap_err().kind()
                ),
                "ResponseContentType"
            );
            assert_eq!(fixture.finish().len(), 1);
        }
    }

    #[test]
    fn openai_responses_https_accepts_complete_terminal_before_http_tail_cut() {
        let body = responses_api_body("text(true);");
        let fixture = TlsFixture::start(vec![TestResponse {
            status: "200 OK",
            content_type: "text/event-stream",
            body: &body,
            declared_length: Some(100_000),
            delay: Duration::ZERO,
        }]);
        let adapter = HttpsSseProviderAdapter::try_new(
            config(&fixture).with_openai_responses(32_768).unwrap(),
        )
        .unwrap();
        let exchange = ModelExchangeId("mdl_00000000000000000000000001".into());
        let request_id = RequestId("req_00000000000000000000000001".into());
        let payload = responses_api_payload();
        let mut request = invocation(&exchange, &request_id);
        request.payload = &payload;
        adapter.open_https(request, SECRET).unwrap();
        let completion = adapter
            .drain_canonical(&responses_api_receipt(exchange, request_id))
            .unwrap();
        assert_eq!(
            completion.terminal.outcome(),
            crate::ProviderGatewayTerminalOutcome::Succeeded
        );
        assert_eq!(fixture.finish().len(), 1);
    }

    #[test]
    fn retry_after_retains_delta_and_http_date_without_copying_secret_headers() {
        let mut headers = ureq::http::HeaderMap::new();
        headers.insert("retry-after", "12".parse().unwrap());
        headers.insert("x-request-id", "provider-request-1".parse().unwrap());
        let facts = response_metadata(&headers, 429, SECRET, &[]);
        assert_eq!(facts.status, Some(429));
        assert_eq!(facts.provider_retry_after_millis, Some(12_000));
        assert_eq!(
            facts.provider_request_id.as_deref(),
            Some("provider-request-1")
        );
        let date = httpdate::fmt_http_date(std::time::SystemTime::now() + Duration::from_mins(1));
        headers.insert("retry-after", date.parse().unwrap());
        let wait = response_metadata(&headers, 503, SECRET, &[])
            .provider_retry_after_millis
            .unwrap();
        assert!((58_000..=60_000).contains(&wait));
        headers.insert("retry-after", "not a date or number".parse().unwrap());
        headers.insert(
            "x-request-id",
            std::str::from_utf8(SECRET).unwrap().parse().unwrap(),
        );
        let facts = response_metadata(&headers, 403, SECRET, &[]);
        assert_eq!(facts.provider_retry_after_millis, None);
        assert_eq!(facts.provider_request_id, None);
    }

    struct TestResponse<'body> {
        status: &'static str,
        content_type: &'static str,
        body: &'body str,
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
        fn start(responses: Vec<TestResponse<'_>>) -> Self {
            Self::start_streaming(responses, Duration::ZERO)
        }

        fn start_streaming(responses: Vec<TestResponse<'_>>, chunk_delay: Duration) -> Self {
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
            let responses = responses
                .into_iter()
                .map(|response| {
                    (
                        response.status,
                        response.content_type,
                        response.body.to_owned(),
                        response.declared_length,
                        response.delay,
                    )
                })
                .collect::<Vec<_>>();
            let server = thread::spawn(move || {
                let config = Arc::new(config);
                for (status, content_type, body, declared_length, delay) in responses {
                    let response = TestResponse {
                        status,
                        content_type,
                        body: &body,
                        declared_length,
                        delay,
                    };
                    let (socket, _) = listener.accept().expect("accept TLS request");
                    let connection =
                        ServerConnection::new(Arc::clone(&config)).expect("TLS connection");
                    let mut stream = StreamOwned::new(connection, socket);
                    let request = read_http_request(&mut stream);
                    request_tx.send(request).expect("record TLS request");
                    thread::sleep(response.delay);
                    write_http_response(&mut stream, &response, chunk_delay);
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

    struct ConnectFixture {
        url: String,
        requests: mpsc::Receiver<Vec<u8>>,
        server: thread::JoinHandle<()>,
    }

    impl ConnectFixture {
        fn start(target: Option<&str>) -> Self {
            let target = target.map(|endpoint| {
                ureq::http::Uri::from_str(endpoint)
                    .expect("target URI")
                    .port_u16()
                    .expect("target port")
            });
            let listener = TcpListener::bind("127.0.0.1:0").expect("bind CONNECT proxy");
            let address = listener.local_addr().expect("proxy address");
            let (request_tx, requests) = mpsc::channel();
            let server = thread::spawn(move || {
                let (mut client, _) = listener.accept().expect("proxy connection");
                client
                    .set_read_timeout(Some(Duration::from_secs(3)))
                    .expect("proxy read timeout");
                let mut request = Vec::new();
                while !request.ends_with(b"\r\n\r\n") {
                    let mut byte = [0];
                    client.read_exact(&mut byte).expect("CONNECT header");
                    request.push(byte[0]);
                    assert!(request.len() < 16 * 1024);
                }
                request_tx.send(request).expect("record CONNECT");
                let Some(port) = target else {
                    let mut byte = [0];
                    assert_eq!(client.read(&mut byte).expect("cancelled CONNECT socket"), 0);
                    return;
                };
                let mut upstream = TcpStream::connect(("127.0.0.1", port)).expect("proxy target");
                client
                    .write_all(b"HTTP/1.1 200 Connection Established\r\n\r\n")
                    .expect("CONNECT response");
                client.set_read_timeout(None).expect("relay timeout");
                let mut read_client = client.try_clone().expect("client relay");
                let mut write_upstream = upstream.try_clone().expect("upstream relay");
                let outbound = thread::spawn(move || {
                    let _ = std::io::copy(&mut read_client, &mut write_upstream);
                    let _ = write_upstream.shutdown(Shutdown::Both);
                });
                let _ = std::io::copy(&mut upstream, &mut client);
                let _ = client.shutdown(Shutdown::Both);
                outbound.join().expect("outbound relay");
            });
            Self {
                url: format!("http://fixture-user:fixture-password@{address}"),
                requests,
                server,
            }
        }

        fn finish(self) -> Vec<Vec<u8>> {
            self.server.join().expect("join CONNECT fixture");
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
        response: &TestResponse<'_>,
        chunk_delay: Duration,
    ) {
        let content_type = if response.content_type.is_empty() {
            String::new()
        } else {
            format!("Content-Type: {}\r\n", response.content_type)
        };
        if write!(
            stream,
            "HTTP/1.1 {}\r\n{}Content-Length: {}\r\nConnection: close\r\n\r\n",
            response.status,
            content_type,
            response.declared_length.unwrap_or(response.body.len()),
        )
        .is_err()
            || stream.flush().is_err()
        {
            return;
        }
        for chunk in response.body.as_bytes().chunks(64) {
            thread::sleep(chunk_delay);
            if stream.write_all(chunk).is_err() || stream.flush().is_err() {
                return;
            }
        }
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
    fn sse_representation_accepts_mime_parameters_and_metadata() {
        for header in [
            "text/event-stream",
            "text/event-stream;charset=utf-8",
            "text/event-stream; charset=\"UTF-8\"",
            "TEXT/EVENT-STREAM;CHARSET=utf-8",
            "text/event-stream; charset=latin1",
            "text/event-stream; x-provider=stream",
        ] {
            assert!(canonical_event_stream_content_type(header), "{header}");
        }
        for header in [
            "application/json",
            "text/event-streaming",
            "text/event-stream, application/json",
        ] {
            assert!(!canonical_event_stream_content_type(header), "{header}");
        }
        let wire =
            successful_sse().replace("data:", "id: 42\nretry: 1000\nx-provider: ignored\ndata:");
        for ending in ["\n", "\r\n", "\r"] {
            let body = format!("\u{feff}{}", wire.replace('\n', ending));
            assert!(parse_sse(body.as_bytes(), 2048, 16).is_ok());
        }
    }

    #[test]
    fn canonical_sse_normalizes_framing_without_changing_event_semantics() {
        for name in ["", "message"] {
            let wire = successful_sse().replace("data:", &format!("event: {name}\ndata:"));
            assert!(parse_sse(wire.as_bytes(), 2048, 16).is_ok());
        }
        for name in ["error", "foreign", "Message"] {
            let wire = successful_sse().replace("data:", &format!("event: {name}\ndata:"));
            assert_eq!(
                parsing_error_kind(parse_sse(wire.as_bytes(), 2048, 16)),
                HttpsSseProviderErrorKind::SseEvent
            );
        }
    }

    #[test]
    fn response_codec_failure_codes_cover_anthropic_and_openai_boundaries() {
        let bindings = AnthropicToolBindings::default();
        let options = AnthropicMessagesOptions {
            max_output_tokens: 32768,
            pricing: ProviderTokenPricing::default(),
        };
        for (body, expected) in [
            (
                &b"data: \xff\n\n"[..],
                HttpsSseProviderErrorKind::SseFraming,
            ),
            (
                &b"data: {invalid}\n\n"[..],
                HttpsSseProviderErrorKind::SseEvent,
            ),
            (&b""[..], HttpsSseProviderErrorKind::IncompleteStream),
        ] {
            let anthropic = parse_anthropic_sse(body, 4096, 32, &bindings, options)
                .err()
                .unwrap();
            let openai =
                crate::provider_openai::parse_openai_chat_sse(body, 4096, 32, &bindings, options)
                    .err()
                    .unwrap();
            for error in [anthropic, openai] {
                assert_eq!(map_anthropic_response(error).kind(), expected);
            }
        }
    }

    #[test]
    fn https_failure_codes_distinguish_open_and_stream_validation() {
        for (status, content_type, body, expected) in [
            (
                "503 Service Unavailable",
                "application/json",
                "{}",
                "Upstream",
            ),
            ("200 OK", "application/json", "{}", "ResponseContentType"),
            (
                "200 OK",
                "text/event-stream",
                "invalid\n",
                "IncompleteStream",
            ),
            (
                "200 OK",
                "text/event-stream",
                "data: {invalid}\n\n",
                "SseEvent",
            ),
            ("200 OK", "text/event-stream", "", "IncompleteStream"),
            (
                "200 OK",
                "text/event-stream",
                concat!(
                    "data: {\"type\":\"text.delta\",\"index\":0,\"delta\":\"x\"}\n\n",
                    "data: {\"type\":\"usage\",\"inputTokens\":1,\"cachedInputTokens\":0,",
                    "\"cacheWriteInputTokens\":0,\"outputTokens\":1,\"reasoningOutputTokens\":0,",
                    "\"actualCostMicros\":0}\n\n",
                    "data: {\"type\":\"response.finished\",\"reason\":\"stop\"}\n\n"
                ),
                "StreamConversion",
            ),
        ] {
            let fixture = TlsFixture::start(vec![TestResponse {
                status,
                content_type,
                body,
                declared_length: None,
                delay: Duration::ZERO,
            }]);
            let adapter = HttpsSseProviderAdapter::try_new(config(&fixture)).unwrap();
            let exchange = ModelExchangeId("mdl_00000000000000000000000001".into());
            let request_id = RequestId("req_00000000000000000000000001".into());
            let result = adapter.open_https(invocation(&exchange, &request_id), SECRET);
            let kind = if let Err(error) = result {
                format!("{:?}", error.kind())
            } else {
                let receipt = ProviderGatewayOpenReceipt {
                    model_exchange_id: exchange,
                    request_id,
                    route: winwincode_api::generated::ModelRoute {
                        provider_id: "provider-https-fixture".into(),
                        model_id: "fixture-model".into(),
                        credential_reference_id: winwincode_domain::CredentialReferenceId(
                            "crd_00000000000000000000000001".into(),
                        ),
                    },
                    adapter_request_id: "pad_00000000000000000000000001".into(),
                    idempotent_replay: false,
                    stream_leak_gate: crate::CredentialLeakGate::new(),
                };
                format!(
                    "{:?}",
                    adapter
                        .drain_canonical(&receipt)
                        .expect_err("failure boundary")
                        .kind()
                )
            };
            assert_eq!(fixture.finish().len(), 1);
            assert_eq!(kind, expected, "{status} / {content_type}");
        }
    }

    #[test]
    fn slow_response_body_uses_the_total_request_deadline() {
        for (total, without_deadlines, succeeds) in [
            (Duration::from_secs(3), false, true),
            (Duration::from_millis(250), false, false),
            (Duration::from_millis(250), true, true),
        ] {
            let fixture = TlsFixture::start_streaming(
                vec![TestResponse {
                    status: "200 OK",
                    content_type: "text/event-stream",
                    body: successful_sse(),
                    declared_length: None,
                    delay: Duration::ZERO,
                }],
                Duration::from_millis(80),
            );
            let mut settings = config(&fixture);
            settings.connect_timeout = total.min(Duration::from_secs(1));
            settings.idle_timeout = Duration::from_millis(250);
            settings.total_timeout = total;
            if without_deadlines {
                settings = settings.without_deadlines();
            }
            let adapter = HttpsSseProviderAdapter::try_new(settings).expect("adapter");
            let io = crate::provider_transport::ExchangeIo::new(
                adapter.shared.config.connect_timeout,
                adapter.shared.config.idle_timeout,
            );
            let agent = HttpsSseProviderAdapter::agent(&adapter.shared.config, Arc::clone(&io));
            let mut response = agent
                .post(&fixture.endpoint)
                .send(PAYLOAD)
                .expect("headers");
            io.body_started();
            let mut body = Vec::new();
            let result = response.body_mut().as_reader().read_to_end(&mut body);
            assert_eq!(
                result.is_ok(),
                succeeds,
                "total={total:?}, without_deadlines={without_deadlines}: {result:?}"
            );
            if succeeds {
                assert_eq!(body, successful_sse().as_bytes());
            }
            drop(response);
            assert_eq!(fixture.finish().len(), 1);
        }
    }

    #[test]
    fn chat_transport_sends_private_headers_and_blocks_echoed_values() {
        for (body, leaks) in [
            (
                "data: {\"id\":\"r1\",\"model\":\"observed-model\",\"choices\":[{\"index\":0,\"delta\":{\"role\":\"assistant\",\"content\":\"\",\"reasoning_content\":\"Check the request.\"}}]}\n\ndata: {\"id\":\"r1\",\"model\":\"observed-model\",\"choices\":[{\"index\":0,\"delta\":{\"content\":\"ok\"},\"finish_reason\":\"stop\"}],\"usage\":{\"prompt_tokens\":2,\"completion_tokens\":1}}\n\ndata: [DONE]\n\n",
                false,
            ),
            (
                "data: {\"id\":\"r1\",\"choices\":[{\"index\":0,\"delta\":{\"content\":\"private-session-value\"},\"finish_reason\":\"stop\"}],\"usage\":{\"prompt_tokens\":2,\"completion_tokens\":1}}\n\ndata: [DONE]\n\n",
                true,
            ),
        ] {
            let fixture = TlsFixture::start(vec![TestResponse {
                status: "200 OK",
                content_type: "text/event-stream",
                body,
                declared_length: None,
                delay: Duration::ZERO,
            }]);
            let settings = config(&fixture)
                .with_openai_chat_completions(8192, ProviderTokenPricing::default())
                .expect("chat")
                .with_custom_headers([(
                    "x-opencode-session".into(),
                    "private-session-value".into(),
                )])
                .expect("headers");
            assert!(!format!("{settings:?}").contains("private-session-value"));
            let adapter = HttpsSseProviderAdapter::try_new(settings).expect("adapter");
            let exchange = ModelExchangeId("mdl_00000000000000000000000001".into());
            let request_id = RequestId("req_00000000000000000000000001".into());
            let payload = serde_json::to_vec(&serde_json::json!({"requestId":"req-1","provider":"p","sessionId":"s","threadId":"t","request":{"model":"requested-model","instructions":"Reply briefly","input":[{"type":"message","role":"user","content":[{"type":"input_text","text":"hello"}]}],"stream":true,"store":false,"tool_choice":"none","parallel_tool_calls":false,"reasoning":{"effort":"max"}}})).expect("payload");
            let mut request = invocation(&exchange, &request_id);
            request.payload = &payload;
            adapter.open_https(request, SECRET).expect("open");
            let receipt = ProviderGatewayOpenReceipt {
                model_exchange_id: exchange,
                request_id,
                route: winwincode_api::generated::ModelRoute {
                    provider_id: "provider-https-fixture".into(),
                    model_id: "fixture-model".into(),
                    credential_reference_id: winwincode_domain::CredentialReferenceId(
                        "crd_00000000000000000000000001".into(),
                    ),
                },
                adapter_request_id: "pad_00000000000000000000000001".into(),
                idempotent_replay: false,
                stream_leak_gate: crate::CredentialLeakGate::new(),
            };
            let result = adapter.drain_canonical(&receipt);
            if leaks {
                assert_eq!(
                    result.expect_err("header echo blocked").kind(),
                    HttpsSseProviderErrorKind::CredentialLeak
                );
            } else {
                assert!(result.is_ok());
            }
            let requests = fixture.finish();
            assert!(contains_ascii_case_insensitive(
                &requests[0],
                b"x-opencode-session: private-session-value"
            ));
            let bytes = &requests[0];
            let start = find_bytes(bytes, b"\r\n\r\n").expect("headers") + 4;
            let body: serde_json::Value =
                serde_json::from_slice(&bytes[start..]).expect("request body");
            assert_eq!(body["reasoning_effort"], "max");
            assert_eq!(body["model"], "fixture-model");
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
    fn explicit_http_connect_proxy_validates_and_redacts_its_route() {
        let fixture = TlsFixture::start(Vec::new());
        let settings = config(&fixture);
        let io = crate::provider_transport::ExchangeIo::new(
            Duration::from_secs(1),
            Duration::from_secs(1),
        );
        assert!(
            HttpsSseProviderAdapter::agent(&settings, io)
                .config()
                .proxy()
                .is_none()
        );
        for url in [
            "proxy.example:8080",
            "https://proxy.example:8080",
            "socks5://proxy.example:8080",
            "http://",
            "http://proxy.example/path",
            "http://proxy.example/?query=secret",
            "http://proxy.example/#fragment",
            "http://proxy.example:not-a-port",
            "http://proxy.example:65536",
            " http://proxy.example",
            "http://proxy.example\n",
        ] {
            assert_eq!(
                settings
                    .clone()
                    .with_http_connect_proxy(url)
                    .expect_err("invalid proxy")
                    .kind(),
                HttpsSseProviderErrorKind::InvalidConfiguration
            );
        }
        let routed = settings
            .with_http_connect_proxy("http://private-user:private-password@localhost:8080")
            .expect("explicit HTTP proxy");
        let debug = format!("{routed:?}");
        assert!(!debug.contains("private-user"));
        assert!(!debug.contains("private-password"));
        assert!(!debug.contains("localhost:8080"));
        fixture.finish();
    }

    #[test]
    fn verified_tls_sse_through_http_connect_preserves_replay_and_private_headers() {
        let fixture = TlsFixture::start(vec![TestResponse {
            status: "200 OK",
            content_type: "text/event-stream",
            body: successful_sse(),
            declared_length: None,
            delay: Duration::ZERO,
        }]);
        let proxy = ConnectFixture::start(Some(&fixture.endpoint));
        let settings = config(&fixture)
            .with_http_connect_proxy(&proxy.url)
            .expect("proxy config");
        let adapter = HttpsSseProviderAdapter::try_new(settings).expect("adapter");
        let exchange = ModelExchangeId("mdl_00000000000000000000000001".into());
        let request_id = RequestId("req_00000000000000000000000001".into());
        let request = invocation(&exchange, &request_id);
        let first = adapter
            .open_https(request, SECRET)
            .expect("proxied TLS open");
        assert_eq!(
            adapter.open_https(request, SECRET).expect("exact replay"),
            first
        );
        let receipt = ProviderGatewayOpenReceipt {
            model_exchange_id: exchange,
            request_id,
            route: winwincode_api::generated::ModelRoute {
                provider_id: "provider-https-fixture".into(),
                model_id: "fixture-model".into(),
                credential_reference_id: winwincode_domain::CredentialReferenceId(
                    "crd_00000000000000000000000001".into(),
                ),
            },
            adapter_request_id: "pad_00000000000000000000000001".into(),
            idempotent_replay: false,
            stream_leak_gate: crate::CredentialLeakGate::new(),
        };
        assert!(matches!(
            adapter
                .drain_canonical(&receipt)
                .expect("proxied SSE")
                .terminal,
            ProviderGatewayTerminal::Completed { .. }
        ));
        let provider_requests = fixture.finish();
        assert_eq!(provider_requests.len(), 1);
        assert!(
            provider_requests[0]
                .windows(SECRET.len())
                .any(|window| window == SECRET)
        );
        assert!(!contains_ascii_case_insensitive(
            &provider_requests[0],
            b"proxy-authorization:"
        ));
        let proxy_requests = proxy.finish();
        assert_eq!(proxy_requests.len(), 1);
        assert!(proxy_requests[0].starts_with(b"CONNECT localhost:"));
        assert!(contains_ascii_case_insensitive(
            &proxy_requests[0],
            b"proxy-authorization: basic "
        ));
        assert!(
            !proxy_requests[0]
                .windows(SECRET.len())
                .any(|window| window == SECRET)
        );
        assert!(
            !proxy_requests[0]
                .windows(PAYLOAD.len())
                .any(|window| window == PAYLOAD)
        );
    }

    #[test]
    fn cancel_and_release_interrupt_a_pending_http_connect_socket() {
        for action in [
            ProviderStreamControlAction::Cancel,
            ProviderStreamControlAction::Release,
        ] {
            let fixture = TlsFixture::start(Vec::new());
            let proxy = ConnectFixture::start(None);
            let adapter = HttpsSseProviderAdapter::try_new(
                config(&fixture)
                    .with_http_connect_proxy(&proxy.url)
                    .expect("proxy config"),
            )
            .expect("adapter");
            let exchange = ModelExchangeId("mdl_00000000000000000000000004".into());
            let request_id = RequestId("req_00000000000000000000000004".into());
            let opener_adapter = adapter.clone();
            let opener_exchange = exchange.clone();
            let opener_request_id = request_id.clone();
            let opener = thread::spawn(move || {
                opener_adapter.open_https(invocation(&opener_exchange, &opener_request_id), SECRET)
            });
            proxy
                .requests
                .recv_timeout(Duration::from_secs(2))
                .expect("pending CONNECT");
            let started = std::time::Instant::now();
            adapter
                .control(&exchange, "pad_00000000000000000000000001", action)
                .expect("interrupt proxy socket");
            assert_eq!(
                opener.join().expect("pending open"),
                Err(ProviderAdapterError::rejected())
            );
            assert!(
                started.elapsed() < Duration::from_secs(1),
                "control must close the TCP socket immediately"
            );
            assert_eq!(
                adapter.open_https(invocation(&exchange, &request_id), SECRET),
                Err(ProviderAdapterError::rejected())
            );
            assert!(fixture.finish().is_empty());
            assert!(proxy.finish().is_empty());
        }
    }

    #[test]
    fn proxied_sse_body_keeps_the_progress_idle_timeout_without_a_total_deadline() {
        let fixture = TlsFixture::start_streaming(
            vec![TestResponse {
                status: "200 OK",
                content_type: "text/event-stream",
                body: successful_sse(),
                declared_length: None,
                delay: Duration::ZERO,
            }],
            Duration::from_millis(350),
        );
        let proxy = ConnectFixture::start(Some(&fixture.endpoint));
        let mut settings = config(&fixture)
            .without_deadlines()
            .with_http_connect_proxy(&proxy.url)
            .expect("proxy config");
        settings.idle_timeout = Duration::from_millis(100);
        let io = crate::provider_transport::ExchangeIo::new(
            settings.connect_timeout,
            settings.idle_timeout,
        );
        let agent = HttpsSseProviderAdapter::agent(&settings, Arc::clone(&io));
        let mut response = agent
            .post(&fixture.endpoint)
            .send(PAYLOAD)
            .expect("proxied headers");
        io.body_started();
        let started = std::time::Instant::now();
        assert!(
            response
                .body_mut()
                .as_reader()
                .read_to_end(&mut Vec::new())
                .is_err()
        );
        assert!(started.elapsed() < Duration::from_secs(1));
        drop(response);
        assert_eq!(fixture.finish().len(), 1);
        assert_eq!(proxy.finish().len(), 1);
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
                status: "408 Request Timeout",
                content_type: "application/json",
                body: "{}",
                declared_length: None,
                delay: Duration::ZERO,
            },
            TestResponse {
                status: "425 Too Early",
                content_type: "application/json",
                body: "{}",
                declared_length: None,
                delay: Duration::ZERO,
            },
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
        for status in [408, 425] {
            let failure = adapter
                .open_https(request, SECRET)
                .expect_err("retryable HTTP")
                .network_failure();
            assert_eq!(failure.http_status, Some(status));
            assert!(failure.retryable());
        }
        assert_eq!(
            adapter.open_https(request, SECRET).expect_err("5xx").kind(),
            crate::ProviderAdapterErrorKind::Upstream
        );
        assert_eq!(
            adapter.open_https(request, SECRET).expect_err("429").kind(),
            crate::ProviderAdapterErrorKind::RateLimited
        );
        adapter
            .open_https(request, SECRET)
            .expect("retry exact idempotency identity");
        let requests = fixture.finish();
        assert_eq!(requests.len(), 5);
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
                    cached_input_tokens: Some(2),
                    cache_write_input_tokens: 3,
                    output_tokens: 5,
                    reasoning_output_tokens: 7,
                },
                actual_cost_micros: Some(19),
            }
        ));
        assert_eq!(
            parsing_error_kind(parse_sse(b"data: {\"type\":\"unknown\"}\n\n", 1024, 2,)),
            HttpsSseProviderErrorKind::SseEvent
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
            ProviderAdapterError::identity_conflict()
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
        let result = HttpsSseProviderConfig::try_new(
            "provider".to_owned(),
            "https://localhost/v1/model#fragment".to_owned(),
            HttpsSseProviderTimeouts {
                connect: Duration::from_secs(1),
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
            result.expect_err("fragment-bearing endpoint").kind(),
            HttpsSseProviderErrorKind::InvalidConfiguration
        );
    }
    #[test]
    fn codex_missing_mime_preserves_account_headers_and_unwraps_request() {
        use winwincode_api::generated::ModelRoute;
        use winwincode_domain::CredentialReferenceId;
        for plan in [false, true] {
            let fixture = TlsFixture::start(vec![TestResponse {
                status: "200 OK",
                content_type: "",
                body: concat!(
                    "data: {\"type\":\"response.created\",\"response\":{\"id\":\"resp-one\"}}\n\n",
                    "data: {\"type\":\"response.output_item.added\",\"item\":{\"type\":\"message\",\"id\":\"msg-one\",\"role\":\"assistant\",\"content\":[]}}\n\n",
                    "data: {\"type\":\"response.output_item.done\",\"item\":{\"type\":\"message\",\"id\":\"msg-one\",\"role\":\"assistant\",\"content\":[{\"type\":\"output_text\",\"text\":\"OK\"}],\"phase\":\"final_answer\"}}\n\n",
                    "data: {\"type\":\"response.completed\",\"response\":{\"id\":\"resp-one\",\"usage\":{\"input_tokens\":2,\"output_tokens\":1}}}\n\n"
                ),
                declared_length: Some(100_000),
                delay: Duration::ZERO,
            }]);
            let mut settings = config(&fixture);
            settings.endpoint = if plan {
                crate::chatgpt_oauth::ENDPOINT
            } else {
                crate::codex_login::CODEX_ENDPOINT
            }
            .into();
            let mut settings = if plan {
                settings.with_chatgpt_plan().unwrap()
            } else {
                settings
                    .with_codex_chatgpt("account-fixture".into())
                    .unwrap()
            };
            // Only this private TLS test substitutes a local endpoint after validating production settings.
            settings.endpoint = fixture.endpoint.clone();
            let adapter = HttpsSseProviderAdapter::try_new(settings).unwrap();
            let exchange = ModelExchangeId("mdl_00000000000000000000000001".into());
            let request_id = RequestId("req_00000000000000000000000001".into());
            let mut request = invocation(&exchange, &request_id);
            request.payload=br#"{"request":{"model":"fixture-model","instructions":"Reply OK","input":[],"stream":true,"store":false}}"#;
            adapter.open_https(request, SECRET).unwrap();
            let mut leak_gate = crate::CredentialLeakGate::new();
            leak_gate.track_secret(&ResolvedSecret::from_bytes(SECRET.to_vec()).unwrap());
            let adapter_request_id = request.adapter_request_id.to_owned();
            let receipt = ProviderGatewayOpenReceipt {
                model_exchange_id: exchange,
                request_id,
                route: ModelRoute {
                    provider_id: "provider-https-fixture".into(),
                    model_id: "fixture-model".into(),
                    credential_reference_id: CredentialReferenceId(
                        "crd_00000000000000000000000001".into(),
                    ),
                },
                adapter_request_id,
                idempotent_replay: false,
                stream_leak_gate: leak_gate,
            };
            assert_eq!(
                adapter
                    .drain_canonical(&receipt)
                    .unwrap()
                    .terminal
                    .outcome(),
                crate::ProviderGatewayTerminalOutcome::Succeeded
            );
            let requests = fixture.finish();
            assert_eq!(
                contains_ascii_case_insensitive(
                    &requests[0],
                    b"ChatGPT-Account-Id: account-fixture"
                ),
                !plan
            );
            assert!(contains_ascii_case_insensitive(
                &requests[0],
                b"Authorization: Bearer provider-https-sse-secret-fixture"
            ));
            let start = find_bytes(&requests[0], b"\r\n\r\n").unwrap() + 4;
            let body: serde_json::Value = serde_json::from_slice(&requests[0][start..]).unwrap();
            assert_eq!(body["model"], "fixture-model");
            assert!(body.get("request").is_none());
        }
    }

    fn responses_fixture_adapter(
        fixture: &TlsFixture,
        plan: bool,
    ) -> (HttpsSseProviderAdapter, ProviderGatewayOpenReceipt) {
        let mut settings = config(fixture);
        settings.endpoint = if plan {
            crate::chatgpt_oauth::ENDPOINT
        } else {
            crate::codex_login::CODEX_ENDPOINT
        }
        .into();
        let mut settings = if plan {
            settings.with_chatgpt_plan().unwrap()
        } else {
            settings
                .with_codex_chatgpt("account-fixture".into())
                .unwrap()
        };
        // Validate the production origin before substituting this private TLS fixture.
        settings.endpoint = fixture.endpoint.clone();
        let adapter = HttpsSseProviderAdapter::try_new(settings).unwrap();
        let exchange = ModelExchangeId("mdl_00000000000000000000000001".into());
        let request_id = RequestId("req_00000000000000000000000001".into());
        let mut request = invocation(&exchange, &request_id);
        request.payload = br#"{"request":{"model":"fixture-model","instructions":"Reply OK","input":[],"stream":true,"store":false}}"#;
        adapter.open_https(request, SECRET).unwrap();
        let mut leak_gate = crate::CredentialLeakGate::new();
        leak_gate.track_secret(&ResolvedSecret::from_bytes(SECRET.to_vec()).unwrap());
        let receipt = ProviderGatewayOpenReceipt {
            model_exchange_id: exchange,
            request_id,
            route: winwincode_api::generated::ModelRoute {
                provider_id: "provider-https-fixture".into(),
                model_id: "fixture-model".into(),
                credential_reference_id: winwincode_domain::CredentialReferenceId(
                    "crd_00000000000000000000000001".into(),
                ),
            },
            adapter_request_id: "pad_00000000000000000000000001".into(),
            idempotent_replay: false,
            stream_leak_gate: leak_gate,
        };
        (adapter, receipt)
    }

    fn failed_responses_events(
        code: &str,
        prefix_usage: bool,
        terminal_usage: bool,
    ) -> Vec<serde_json::Value> {
        let mut events = vec![serde_json::json!({
            "type": "response.created", "response": { "id": "failed-response" },
        })];
        if prefix_usage {
            events.push(serde_json::json!({
                "type": "response.in_progress", "response": {
                    "id": "failed-response", "usage": {
                        "input_tokens": 7, "output_tokens": 3,
                        "input_tokens_details": { "cached_tokens": 2 },
                        "output_tokens_details": { "reasoning_tokens": 1 },
                    },
                },
            }));
        }
        events.push(serde_json::json!({
            "type": "response.output_text.delta", "delta": "partial output",
        }));
        let mut terminal = serde_json::json!({
            "type": "response.failed", "response": {
                "id": "failed-response",
                "error": { "code": code, "message": "private failure diagnostic" },
            },
        });
        if terminal_usage {
            terminal["response"]["usage"] = serde_json::json!({
                "input_tokens": 10, "output_tokens": 4,
                "input_tokens_details": { "cached_tokens": 2 },
                "output_tokens_details": { "reasoning_tokens": 1 },
            });
        }
        events.push(terminal);
        events
    }

    fn responses_wire(events: &[serde_json::Value]) -> String {
        use std::fmt::Write as _;
        let mut wire = String::new();
        for event in events {
            writeln!(wire, "data: {event}\n").unwrap();
        }
        wire
    }

    fn assert_failed_responses_usage(
        completion: &HttpsSseProviderCompletion,
        input_tokens: u64,
        output_tokens: u64,
    ) {
        let charge = completion.terminal.charge().expect("observed paid usage");
        assert_eq!(charge.usage.input_tokens, input_tokens);
        assert_eq!(charge.usage.output_tokens, output_tokens);
        assert_eq!(charge.usage.cached_input_tokens, Some(2));
        assert_eq!(charge.usage.reasoning_output_tokens, 1);
        assert_eq!(charge.actual_cost_micros, None);
        let frame = completion.frames.last().expect("canonical failed frame");
        assert!(frame.is_terminal());
        let value: serde_json::Value = serde_json::from_str(frame.payload_json()).unwrap();
        assert_eq!(value["tokenUsage"]["input_tokens"], input_tokens);
        assert_eq!(value["tokenUsage"]["output_tokens"], output_tokens);
        assert_eq!(value["tokenUsage"]["cached_input_tokens"], 2);
        assert_eq!(value["tokenUsage"]["reasoning_output_tokens"], 1);
        assert_eq!(
            value["tokenUsage"]["total_tokens"],
            input_tokens + output_tokens
        );
    }

    #[test]
    fn responses_http_tail_cut_preserves_failed_terminal_classification_and_usage() {
        for plan in [false, true] {
            for (code, category, expected_code) in [
                (
                    "invalid_api_key",
                    crate::ModelAttemptFailureKind::Authentication,
                    "AUTH",
                ),
                (
                    "insufficient_quota",
                    crate::ModelAttemptFailureKind::Quota,
                    "QUOTA",
                ),
                (
                    "context_length_exceeded",
                    crate::ModelAttemptFailureKind::ContextWindowExceeded,
                    "CONTEXT_WINDOW_EXCEEDED",
                ),
                (
                    "rate_limit_exceeded",
                    crate::ModelAttemptFailureKind::RateLimit,
                    "RATE_LIMIT",
                ),
            ] {
                let body = responses_wire(&failed_responses_events(code, false, true));
                let fixture = TlsFixture::start(vec![TestResponse {
                    status: "200 OK",
                    content_type: "text/event-stream",
                    body: &body,
                    declared_length: Some(body.len() + 100),
                    delay: Duration::ZERO,
                }]);
                let (adapter, receipt) = responses_fixture_adapter(&fixture, plan);
                let result = adapter.drain_canonical(&receipt);
                assert_eq!(fixture.finish().len(), 1, "one actual HTTPS attempt");
                let completion = result.expect("complete Failed must survive HTTP tail-cut");
                let ProviderGatewayTerminal::Failed { failure, .. } = completion.terminal else {
                    panic!("expected a canonical Failed terminal for {code}, plan={plan}");
                };
                assert_eq!(failure.kind, category);
                assert_eq!(failure.certainty, ModelExecutionCertainty::OutputObserved);
                assert_failed_responses_usage(&completion, 10, 4);
                let last = completion.frames.last().unwrap().payload_json();
                let value: serde_json::Value = serde_json::from_str(last).unwrap();
                assert_eq!(value["error"]["code"], expected_code);
                assert!(!last.contains("private failure diagnostic"));
                assert!(!last.contains(std::str::from_utf8(SECRET).unwrap()));
            }
        }
    }

    #[test]
    fn responses_http_tail_cut_preserves_prefix_usage_and_prefers_terminal_usage() {
        for plan in [false, true] {
            for terminal_usage in [false, true] {
                let body = responses_wire(&failed_responses_events(
                    "insufficient_quota",
                    true,
                    terminal_usage,
                ));
                let fixture = TlsFixture::start(vec![TestResponse {
                    status: "200 OK",
                    content_type: "text/event-stream",
                    body: &body,
                    declared_length: Some(body.len() + 100),
                    delay: Duration::ZERO,
                }]);
                let (adapter, receipt) = responses_fixture_adapter(&fixture, plan);
                let result = adapter.drain_canonical(&receipt);
                assert_eq!(fixture.finish().len(), 1);
                let completion = result.expect("complete Failed retains known usage");
                let ProviderGatewayTerminal::Failed { failure, .. } = completion.terminal else {
                    panic!("expected a canonical Failed terminal");
                };
                assert_eq!(failure.kind, crate::ModelAttemptFailureKind::Quota);
                assert_eq!(failure.certainty, ModelExecutionCertainty::OutputObserved);
                let (input, output) = if terminal_usage { (10, 4) } else { (7, 3) };
                assert_failed_responses_usage(&completion, input, output);
            }
        }
    }

    #[test]
    fn responses_http_tail_cut_does_not_recover_after_cancellation() {
        for plan in [false, true] {
            let body = responses_wire(&failed_responses_events("insufficient_quota", false, true));
            let fixture = TlsFixture::start(vec![TestResponse {
                status: "200 OK",
                content_type: "text/event-stream",
                body: &body,
                declared_length: Some(body.len() + 100),
                delay: Duration::ZERO,
            }]);
            let (adapter, receipt) = responses_fixture_adapter(&fixture, plan);
            assert_eq!(fixture.finish().len(), 1);
            // Set the late task cancellation flag without interrupting the already-sent
            // fixture bytes, so the recovery gate must reject a complete Failed body.
            let cancellation = Arc::new(crate::provider_transport::ExchangeCancellation::default());
            let adapter = adapter.with_cancellation(Arc::clone(&cancellation));
            cancellation.cancel();
            let error = adapter
                .drain_canonical(&receipt)
                .expect_err("task cancellation takes priority over a parsed Failed terminal");
            assert_eq!(error.kind(), HttpsSseProviderErrorKind::Transport);
            let (response_id, usage) = error.observed_receipt().expect("actual HTTP body was read");
            assert_eq!(response_id, "failed-response");
            assert_eq!(usage.input_tokens, 10);
            assert_eq!(usage.output_tokens, 4);
        }
    }

    #[test]
    fn responses_http_tail_cut_recovery_keeps_identity_leak_and_structure_gates() {
        for plan in [false, true] {
            for fault in [
                "identity",
                "prefix_identity",
                "prefix_without_identity",
                "prefix_usage",
                "credential",
                "after_terminal",
            ] {
                let mut events = failed_responses_events("insufficient_quota", true, true);
                match fault {
                    "identity" => {
                        events.last_mut().unwrap()["response"]["id"] = "foreign-response".into();
                    }
                    "prefix_identity" => {
                        events[1]["response"]["id"] = "foreign-response".into();
                    }
                    "prefix_without_identity" => {
                        events[1]["response"].as_object_mut().unwrap().remove("id");
                    }
                    "prefix_usage" => {
                        events[1]["response"]["usage"]["input_tokens"] = "invalid".into();
                    }
                    "credential" => {
                        events[2]["delta"] = std::str::from_utf8(SECRET).unwrap().into();
                    }
                    "after_terminal" => events.push(serde_json::json!({
                        "type": "response.output_text.delta", "delta": "late output",
                    })),
                    _ => unreachable!(),
                }
                let body = responses_wire(&events);
                let fixture = TlsFixture::start(vec![TestResponse {
                    status: "200 OK",
                    content_type: "text/event-stream",
                    body: &body,
                    declared_length: Some(body.len() + 100),
                    delay: Duration::ZERO,
                }]);
                let (adapter, receipt) = responses_fixture_adapter(&fixture, plan);
                let result = adapter.drain_canonical(&receipt);
                assert_eq!(fixture.finish().len(), 1);
                let error =
                    result.expect_err("invalid response cannot become canonical completion");
                assert_eq!(
                    error.kind(),
                    HttpsSseProviderErrorKind::Transport,
                    "{fault}, plan={plan}"
                );
                assert!(!format!("{error:?}").contains(std::str::from_utf8(SECRET).unwrap()));
            }
        }
    }

    #[test]
    fn chatgpt_plan_uses_the_public_endpoint_protocol_without_codex_headers() {
        for content_type in ["text/event-stream", "", "application/json"] {
            let fixture = TlsFixture::start(vec![TestResponse {
                status: "200 OK",
                content_type,
                body: "data: {}\n\n",
                declared_length: None,
                delay: Duration::ZERO,
            }]);
            assert!(config(&fixture).with_chatgpt_plan().is_err());
            let mut settings = config(&fixture);
            settings.endpoint = crate::chatgpt_oauth::ENDPOINT.into();
            let mut settings = settings.with_chatgpt_plan().unwrap();
            // Production pins are validated before substituting this private TLS fixture.
            settings.endpoint = fixture.endpoint.clone();
            let adapter = HttpsSseProviderAdapter::try_new(settings).unwrap();
            let exchange = ModelExchangeId("mdl_00000000000000000000000001".into());
            let request_id = RequestId("req_00000000000000000000000001".into());
            let mut request = invocation(&exchange, &request_id);
            request.payload = br#"{"request":{"model":"fixture-model","input":[],"stream":false,"store":true,"max_output_tokens":128}}"#;
            assert_eq!(
                adapter.open_https(request, SECRET).is_ok(),
                content_type != "application/json"
            );
            let requests = fixture.finish();
            assert!(!contains_ascii_case_insensitive(
                &requests[0],
                b"ChatGPT-Account-Id:"
            ));
            assert!(!contains_ascii_case_insensitive(
                &requests[0],
                b"originator:"
            ));
            assert!(contains_ascii_case_insensitive(
                &requests[0],
                b"Authorization: Bearer provider-https-sse-secret-fixture"
            ));
            let start = find_bytes(&requests[0], b"\r\n\r\n").unwrap() + 4;
            let body: serde_json::Value = serde_json::from_slice(&requests[0][start..]).unwrap();
            assert_eq!(body["max_output_tokens"], 128);
            assert_eq!(body["stream"], true);
            assert_eq!(body["store"], false);
            assert!(body.get("request").is_none());
        }
    }
}

#[cfg(test)]
mod custom_header_tests {
    use super::validate_custom_headers;

    #[test]
    fn custom_headers_reject_transport_overrides_injection_and_duplicates() {
        assert!(
            validate_custom_headers(&[("x-opencode-session".into(), "private-session".into())])
                .is_ok()
        );
        for (name, value) in [
            ("Authorization", "Bearer secret"),
            ("Host", "another.host"),
            ("Content-Type", "text/plain"),
            ("safe", "x\r\nInjected: yes"),
            ("bad name", "value"),
        ] {
            assert!(validate_custom_headers(&[(name.into(), value.into())]).is_err());
        }
        assert!(
            validate_custom_headers(&[
                ("X-Session".into(), "a".into()),
                ("x-session".into(), "b".into())
            ])
            .is_err()
        );
    }
}
