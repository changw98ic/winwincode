use crate::rate_limits::RateLimitError;
use codex_client::TransportError;
use http::StatusCode;
use std::time::Duration;
use thiserror::Error;

#[derive(Debug, Error)]
pub enum ApiError {
    /// The host exhausted its physical attempt policy. Core must consume the
    /// original semantic failure rather than start a second retry budget.
    #[error(transparent)]
    HostTerminal { source: Box<ApiError> },
    /// The embedding host rejected work under the original expired execution lease.
    #[error("execution lease expired")]
    ExecutionLeaseExpired,
    /// The host refused a model call because its original execution authority expired.
    #[error("model call authority expired")]
    ModelAuthorityExpired,
    #[error(transparent)]
    Transport(#[from] TransportError),
    #[error("api error {status}: {message}")]
    Api { status: StatusCode, message: String },
    #[error("stream error: {0}")]
    Stream(String),
    #[error("context window exceeded")]
    ContextWindowExceeded,
    #[error("quota exceeded")]
    QuotaExceeded,
    #[error("usage not included")]
    UsageNotIncluded,
    #[error("retryable error: {message}")]
    Retryable {
        message: String,
        delay: Option<Duration>,
    },
    #[error("rate limit: {0}")]
    RateLimit(String),
    #[error("invalid request: {message}")]
    InvalidRequest { message: String },
    #[error("cyber policy: {message}")]
    CyberPolicy { message: String },
    #[error("misalignment policy violation: {message}")]
    MisalignmentPolicyViolation { message: String },
    #[error("server overloaded")]
    ServerOverloaded,
}

impl From<RateLimitError> for ApiError {
    fn from(err: RateLimitError) -> Self {
        Self::RateLimit(err.to_string())
    }
}

impl ApiError {
    pub fn without_retry(self) -> Self {
        Self::HostTerminal {
            source: Box::new(self),
        }
    }
}
