// SPDX-License-Identifier: Apache-2.0

//! Payload-free diagnostic facts. Never retain transport Display strings here:
//! those can contain URLs, headers, credentials or upstream response text.
use serde::{Deserialize, Serialize};

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum DiagnosticCode {
    HttpStatus,
    Dns,
    Connect,
    Io,
    Timeout,
    Tls,
    TlsCertificate,
    HttpProtocol,
    RequestUri,
    RequestHeaders,
    Proxy,
    Redirect,
    ResponseHeadersTooLarge,
    BodyTooLarge,
    ContentType,
    TransportOther,
    JsonSyntax,
    JsonSchema,
    ResponseInvariant,
    SchemaVersion,
    SseFraming,
    SseEvent,
    StreamConversion,
    StreamIncomplete,
    EmptyResponse,
    IdentityConflict,
    CredentialBlocked,
    Storage,
    Configuration,
    AuthorityEnded,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum IoFailureKind {
    TimedOut,
    ConnectionRefused,
    NotConnected,
    NetworkUnreachable,
    HostUnreachable,
    UnexpectedEof,
    ConnectionReset,
    ConnectionAborted,
    BrokenPipe,
    Interrupted,
    InvalidData,
    PermissionDenied,
    Other,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum DiagnosticField {
    ContentType,
    Model,
    Answers,
    AnswerType,
    Probabilities,
    Choice,
    Confidence,
    UsageInputTokens,
    UsageOutputTokens,
    Scores,
    Labels,
    Entailment,
    Contradiction,
    Neutral,
}

/// A generated response-log filename, never an arbitrary path.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct SseResponseLogId([u8; 32]);

impl SseResponseLogId {
    pub fn parse(value: &str) -> Option<Self> {
        let hex = value.strip_prefix("sse-")?.strip_suffix(".log")?;
        if hex.len() != 64
            || !hex
                .bytes()
                .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
        {
            return None;
        }
        let mut bytes = [0; 32];
        for (index, byte) in bytes.iter_mut().enumerate() {
            *byte = u8::from_str_radix(&hex[index * 2..index * 2 + 2], 16).ok()?;
        }
        Some(Self(bytes))
    }
}
impl std::fmt::Display for SseResponseLogId {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("sse-")?;
        for byte in self.0 {
            write!(formatter, "{byte:02x}")?;
        }
        formatter.write_str(".log")
    }
}
impl Serialize for SseResponseLogId {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.collect_str(self)
    }
}
impl<'de> Deserialize<'de> for SseResponseLogId {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let value = String::deserialize(deserializer)?;
        Self::parse(&value)
            .ok_or_else(|| serde::de::Error::custom("invalid response log reference"))
    }
}
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ResponseLogStatus {
    Retained,
    WriteFailed,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct NetworkDiagnostic {
    pub code: DiagnosticCode,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub field: Option<DiagnosticField>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub io_kind: Option<IoFailureKind>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub os_code: Option<i32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub line: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub column: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub response_log: Option<SseResponseLogId>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub response_log_status: Option<ResponseLogStatus>,
}

impl NetworkDiagnostic {
    pub const fn new(code: DiagnosticCode) -> Self {
        Self {
            code,
            field: None,
            io_kind: None,
            os_code: None,
            line: None,
            column: None,
            response_log: None,
            response_log_status: None,
        }
    }

    pub fn io(error: &std::io::Error) -> Self {
        use std::io::ErrorKind as K;
        let io_kind = match error.kind() {
            K::TimedOut => IoFailureKind::TimedOut,
            K::ConnectionRefused => IoFailureKind::ConnectionRefused,
            K::NotConnected => IoFailureKind::NotConnected,
            K::NetworkUnreachable => IoFailureKind::NetworkUnreachable,
            K::HostUnreachable => IoFailureKind::HostUnreachable,
            K::UnexpectedEof => IoFailureKind::UnexpectedEof,
            K::ConnectionReset => IoFailureKind::ConnectionReset,
            K::ConnectionAborted => IoFailureKind::ConnectionAborted,
            K::BrokenPipe => IoFailureKind::BrokenPipe,
            K::Interrupted => IoFailureKind::Interrupted,
            K::InvalidData => IoFailureKind::InvalidData,
            K::PermissionDenied => IoFailureKind::PermissionDenied,
            _ => IoFailureKind::Other,
        };
        Self {
            io_kind: Some(io_kind),
            os_code: error.raw_os_error(),
            ..Self::new(DiagnosticCode::Io)
        }
    }

    pub fn json(error: &serde_json::Error) -> Self {
        Self {
            line: u32::try_from(error.line()).ok(),
            column: u32::try_from(error.column()).ok(),
            ..Self::new(if error.is_data() {
                DiagnosticCode::JsonSchema
            } else {
                DiagnosticCode::JsonSyntax
            })
        }
    }
}
