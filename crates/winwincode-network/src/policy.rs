// SPDX-License-Identifier: Apache-2.0

//! Network facts contain no URLs, credentials, request bodies or upstream text.

use serde::{Deserialize, Serialize};
use std::{sync::OnceLock, time::Duration};

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ErrorKind {
    ConnectionUnavailable,
    TransportInterrupted,
    Timeout,
    RateLimited,
    ServerTransient,
    Authentication,
    Authorization,
    RequestInvalid,
    ProtocolInvalid,
    IntegrityInvalid,
    TlsInvalid,
    StreamIncomplete,
    EmptyResponse,
    Cancelled,
    AuthorityExpired,
    StorageUnavailable,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Acceptance {
    NotSent,
    Unknown,
    ResponseReceived,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Replay {
    /// Provider calls retain a finite budget once the request may have been sent.
    RetryInference,
    /// Durable control messages retry temporary failures until authority ends.
    ReplayExact,
    ReconcileFirst,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Phase {
    Connect,
    ResponseHeaders,
    ResponseBody,
    Stream,
    Decode,
    Persist,
}

/// Safe error metadata for both attempt ledgers and protocol adapters.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct NetworkFailure {
    pub kind: ErrorKind,
    pub acceptance: Acceptance,
    pub phase: Phase,
    pub http_status: Option<u16>,
    pub retry_after_ms: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub diagnostic: Option<crate::NetworkDiagnostic>,
}

impl std::fmt::Display for NetworkFailure {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            formatter,
            "network {:?} at {:?}; acceptance={:?}; status={:?}",
            self.kind, self.phase, self.acceptance, self.http_status
        )?;
        if let Some(diagnostic) = &self.diagnostic {
            write!(formatter, "; diagnostic={diagnostic:?}")?;
        }
        Ok(())
    }
}

impl std::error::Error for NetworkFailure {}

impl NetworkFailure {
    pub const fn new(kind: ErrorKind, acceptance: Acceptance, phase: Phase) -> Self {
        Self {
            kind,
            acceptance,
            phase,
            http_status: None,
            retry_after_ms: None,
            diagnostic: None,
        }
    }

    #[must_use]
    pub const fn with_diagnostic(mut self, diagnostic: crate::NetworkDiagnostic) -> Self {
        self.diagnostic = Some(diagnostic);
        self
    }

    pub fn http(status: u16, retry_after: Option<Duration>) -> Self {
        let kind = match status {
            401 => ErrorKind::Authentication,
            403 => ErrorKind::Authorization,
            408 => ErrorKind::Timeout,
            425 | 500..=599 => ErrorKind::ServerTransient,
            429 => ErrorKind::RateLimited,
            _ => ErrorKind::RequestInvalid,
        };
        Self {
            http_status: Some(status),
            retry_after_ms: retry_after.map(duration_millis),
            diagnostic: Some(crate::NetworkDiagnostic::new(
                crate::DiagnosticCode::HttpStatus,
            )),
            ..Self::new(kind, Acceptance::ResponseReceived, Phase::ResponseHeaders)
        }
    }

    pub fn retryable(self) -> bool {
        defaults().transient_kinds.contains(&self.kind)
    }
}

#[derive(Clone, Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Defaults {
    pub version: String,
    pub max_attempts: u32,
    pub initial_delay_ms: u64,
    pub max_connection_delay_ms: u64,
    pub max_immediate_wait_ms: u64,
    pub authority_check_ms: u64,
    pub jitter_ms: u64,
    pub transient_kinds: Vec<ErrorKind>,
}

/// Returns the build-time request policy shared with JavaScript.
///
/// # Panics
/// Panics if the embedded, checked policy artifact is malformed.
pub fn defaults() -> &'static Defaults {
    static DEFAULTS: OnceLock<Defaults> = OnceLock::new();
    DEFAULTS.get_or_init(|| {
        serde_json::from_str(include_str!(
            "../../../schema/winwincode/network-request-policy.json"
        ))
        .expect("checked network request policy")
    })
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum RetryDecision {
    RetryAfter(Duration),
    DeferredUntil(Duration),
    Reconcile,
    Stop,
}

/// A pure decision; queue adapters persist the chosen delay before rescheduling.
/// `connection_attempt` counts only failures with trusted proof that nothing was sent.
pub fn decide(
    failure: NetworkFailure,
    replay: Replay,
    attempt: u32,
    connection_attempt: u32,
    max_attempts: u32,
    jitter_ms: u64,
) -> RetryDecision {
    if !failure.retryable() {
        return RetryDecision::Stop;
    }
    if replay == Replay::ReconcileFirst && failure.acceptance != Acceptance::NotSent {
        return RetryDecision::Reconcile;
    }
    let disconnected = failure.acceptance == Acceptance::NotSent
        && matches!(
            failure.kind,
            ErrorKind::ConnectionUnavailable | ErrorKind::Timeout
        );
    // Exact control frames have durable idempotency and live authority checks.
    // A temporary outage must not terminate their owning Device or Worker.
    let durable_control = replay == Replay::ReplayExact;
    if !disconnected && !durable_control && attempt >= max_attempts.max(1) {
        return RetryDecision::Stop;
    }
    let ordinal = if disconnected {
        connection_attempt.max(1)
    } else {
        attempt.max(1)
    };
    let mut delay = exponential_delay(
        ordinal,
        Duration::from_millis(defaults().initial_delay_ms),
        if disconnected || durable_control {
            Duration::from_millis(defaults().max_connection_delay_ms)
        } else {
            Duration::MAX
        },
    );
    if let Some(floor) = failure.retry_after_ms {
        delay = delay.max(Duration::from_millis(floor));
    }
    delay = delay.saturating_add(Duration::from_millis(jitter_ms.min(defaults().jitter_ms)));
    if delay > Duration::from_millis(defaults().max_immediate_wait_ms) {
        RetryDecision::DeferredUntil(delay)
    } else {
        RetryDecision::RetryAfter(delay)
    }
}

pub fn exponential_delay(ordinal: u32, initial: Duration, cap: Duration) -> Duration {
    initial
        .saturating_mul(2_u32.saturating_pow(ordinal.saturating_sub(1)))
        .min(cap)
}

pub fn retry_after(value: &str, now: std::time::SystemTime) -> Option<Duration> {
    if let Ok(seconds) = value.trim().parse::<u64>() {
        return Some(Duration::from_secs(seconds));
    }
    if let Ok(at) = httpdate::parse_http_date(value.trim()) {
        return Some(at.duration_since(now).unwrap_or_default());
    }
    // httpdate rejects dates before the Unix epoch. A valid past date still
    // means that the server's cooldown has already elapsed.
    let at =
        time::OffsetDateTime::parse(value.trim(), &time::format_description::well_known::Rfc2822)
            .ok()?;
    let timestamp = at.unix_timestamp();
    if timestamp < 0 {
        return Some(Duration::ZERO);
    }
    let at =
        std::time::UNIX_EPOCH.checked_add(Duration::from_secs(u64::try_from(timestamp).ok()?))?;
    Some(at.duration_since(now).unwrap_or_default())
}

/// A whole-second lower bound for protocols that persist integer cooldowns.
pub fn retry_after_seconds(value: &str) -> Option<u64> {
    retry_after(value, std::time::SystemTime::now()).map(|delay| {
        delay
            .as_secs()
            .saturating_add(u64::from(delay.subsec_nanos() != 0))
    })
}

pub fn classify_ureq(error: &ureq::Error, not_sent: bool, phase: Phase) -> NetworkFailure {
    let kind = match error {
        ureq::Error::StatusCode(status) => return NetworkFailure::http(*status, None),
        ureq::Error::Timeout(_) => ErrorKind::Timeout,
        ureq::Error::HostNotFound | ureq::Error::ConnectionFailed => {
            ErrorKind::ConnectionUnavailable
        }
        ureq::Error::Io(error) => match error.kind() {
            std::io::ErrorKind::TimedOut => ErrorKind::Timeout,
            std::io::ErrorKind::ConnectionRefused
            | std::io::ErrorKind::NotConnected
            | std::io::ErrorKind::NetworkUnreachable
            | std::io::ErrorKind::HostUnreachable => ErrorKind::ConnectionUnavailable,
            std::io::ErrorKind::UnexpectedEof
            | std::io::ErrorKind::ConnectionReset
            | std::io::ErrorKind::ConnectionAborted
            | std::io::ErrorKind::BrokenPipe
            | std::io::ErrorKind::Interrupted => ErrorKind::TransportInterrupted,
            _ => ErrorKind::ProtocolInvalid,
        },
        ureq::Error::Tls(_) | ureq::Error::Rustls(_) | ureq::Error::Pem(_) => ErrorKind::TlsInvalid,
        ureq::Error::BadUri(_)
        | ureq::Error::Http(_)
        | ureq::Error::InvalidProxyUrl
        | ureq::Error::RequireHttpsOnly(_)
        | ureq::Error::BodyExceedsLimit(_) => ErrorKind::RequestInvalid,
        _ => ErrorKind::ProtocolInvalid,
    };
    let diagnostic = match error {
        ureq::Error::Io(error) => crate::NetworkDiagnostic::io(error),
        other => crate::NetworkDiagnostic::new(match other {
            ureq::Error::HostNotFound => crate::DiagnosticCode::Dns,
            ureq::Error::ConnectionFailed => crate::DiagnosticCode::Connect,
            ureq::Error::Timeout(_) => crate::DiagnosticCode::Timeout,
            ureq::Error::Tls(_) | ureq::Error::Rustls(_) => crate::DiagnosticCode::Tls,
            ureq::Error::Pem(_) => crate::DiagnosticCode::TlsCertificate,
            ureq::Error::Protocol(_) => crate::DiagnosticCode::HttpProtocol,
            ureq::Error::BadUri(_) | ureq::Error::RequireHttpsOnly(_) => {
                crate::DiagnosticCode::RequestUri
            }
            ureq::Error::Http(_) => crate::DiagnosticCode::RequestHeaders,
            ureq::Error::InvalidProxyUrl | ureq::Error::ConnectProxyFailed(_) => {
                crate::DiagnosticCode::Proxy
            }
            ureq::Error::RedirectFailed | ureq::Error::TooManyRedirects => {
                crate::DiagnosticCode::Redirect
            }
            ureq::Error::LargeResponseHeader(_, _) => {
                crate::DiagnosticCode::ResponseHeadersTooLarge
            }
            ureq::Error::BodyExceedsLimit(_) => crate::DiagnosticCode::BodyTooLarge,
            _ => crate::DiagnosticCode::TransportOther,
        }),
    };
    NetworkFailure::new(
        kind,
        if not_sent {
            Acceptance::NotSent
        } else {
            Acceptance::Unknown
        },
        phase,
    )
    .with_diagnostic(diagnostic)
}

pub fn duration_millis(duration: Duration) -> u64 {
    u64::try_from(duration.as_millis()).unwrap_or(u64::MAX)
}

/// Classifies reqwest errors from trusted transport facts, never from provider text.
#[cfg(feature = "reqwest")]
pub fn classify_reqwest(error: &reqwest::Error, phase: Phase) -> NetworkFailure {
    use std::error::Error as _;
    let mut source = error.source();
    let mut diagnostic = crate::NetworkDiagnostic::new(if error.is_builder() {
        crate::DiagnosticCode::RequestUri
    } else if error.is_timeout() {
        crate::DiagnosticCode::Timeout
    } else if error.is_connect() {
        crate::DiagnosticCode::Connect
    } else if error.is_decode() {
        crate::DiagnosticCode::HttpProtocol
    } else {
        crate::DiagnosticCode::TransportOther
    });
    while let Some(cause) = source {
        if cause.is::<rustls::Error>() {
            return NetworkFailure::new(ErrorKind::TlsInvalid, Acceptance::NotSent, Phase::Connect)
                .with_diagnostic(crate::NetworkDiagnostic::new(crate::DiagnosticCode::Tls));
        }
        if let Some(error) = cause.downcast_ref::<std::io::Error>() {
            diagnostic = crate::NetworkDiagnostic::io(error);
        }
        source = cause.source();
    }
    let kind = if error.is_builder() {
        ErrorKind::RequestInvalid
    } else if error.is_timeout() {
        ErrorKind::Timeout
    } else if error.is_connect() {
        ErrorKind::ConnectionUnavailable
    } else if error.is_decode() && !matches!(phase, Phase::ResponseBody | Phase::Stream) {
        ErrorKind::ProtocolInvalid
    } else {
        ErrorKind::TransportInterrupted
    };
    NetworkFailure::new(
        kind,
        if error.is_connect() {
            Acceptance::NotSent
        } else {
            Acceptance::Unknown
        },
        phase,
    )
    .with_diagnostic(diagnostic)
}
