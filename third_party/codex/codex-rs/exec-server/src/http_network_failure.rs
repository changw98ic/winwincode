//! Bounded transport evidence shared across local and remote HTTP capabilities.
use serde::{Deserialize, Serialize};

#[derive(Clone, Copy, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum HttpNetworkErrorKind {
    ConnectionUnavailable,
    TransportInterrupted,
    Timeout,
    TlsInvalid,
    RequestInvalid,
    Authorization,
}

#[derive(Clone, Copy, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct HttpNetworkFailure {
    pub kind: HttpNetworkErrorKind,
    pub not_sent: bool,
}

impl std::fmt::Display for HttpNetworkFailure {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "HTTP network {:?};not_sent={}", self.kind, self.not_sent)
    }
}
impl std::error::Error for HttpNetworkFailure {}

impl HttpNetworkFailure {
    pub(crate) fn from_request(error: &codex_http_client::RouteAwareRequestError) -> Self {
        use codex_http_client::{RouteAwareRequestError, RouteFailureClass};
        let kind = if error.failure_class() == Some(RouteFailureClass::TlsError) {
            HttpNetworkErrorKind::TlsInvalid
        } else if matches!(error, RouteAwareRequestError::Policy(_)) {
            HttpNetworkErrorKind::Authorization
        } else if error.is_builder()
            || matches!(
                error,
                RouteAwareRequestError::Build(_)
                    | RouteAwareRequestError::UnsupportedRedirectScheme(_)
                    | RouteAwareRequestError::TooManyRedirects
            )
        {
            HttpNetworkErrorKind::RequestInvalid
        } else if error.is_timeout() {
            HttpNetworkErrorKind::Timeout
        } else if error.is_connect() {
            HttpNetworkErrorKind::ConnectionUnavailable
        } else {
            HttpNetworkErrorKind::TransportInterrupted
        };
        Self {
            kind,
            not_sent: error.is_connect()
                || matches!(
                    kind,
                    HttpNetworkErrorKind::TlsInvalid
                        | HttpNetworkErrorKind::Authorization
                        | HttpNetworkErrorKind::RequestInvalid
                ),
        }
    }
    pub(crate) fn from_rpc(error: &codex_exec_server_protocol::JSONRPCErrorError) -> Option<Self> {
        serde_json::from_value(error.data.as_ref()?.get("httpNetworkFailure")?.clone()).ok()
    }
    pub(crate) fn into_rpc(self) -> codex_exec_server_protocol::JSONRPCErrorError {
        let mut error = crate::rpc::internal_error(self.to_string());
        error.data = Some(serde_json::json!({"httpNetworkFailure": self}));
        error
    }
}
