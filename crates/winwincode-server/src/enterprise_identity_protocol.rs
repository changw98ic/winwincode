// SPDX-License-Identifier: Apache-2.0

//! Secret-safe HTTP composition for external identity protocols.

use std::fmt;
use std::sync::Arc;

use axum::Router;
use axum::body::{Body, to_bytes};
use axum::extract::State;
use axum::http::header::{AUTHORIZATION, CACHE_CONTROL, CONTENT_TYPE, COOKIE, SET_COOKIE};
use axum::http::{HeaderMap, HeaderValue, Request, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::post;
use base64::{Engine as _, engine::general_purpose::STANDARD};
use serde::Deserialize;
use serde_json::json;
use winwincode_control_plane::{
    EnterpriseIdentityProtocolAdapter, EnterpriseProtocolError, EnterpriseProtocolErrorKind,
    OidcIdToken, SamlResponse, ScimBearerToken, ScimLifecycleEvent,
};

use crate::{
    ExternalIdentitySessionIssuer, ExternalIdentitySessionResult, SqliteAuthSessionManager,
};

const SUPPORTED_SCHEMA_VERSION: &str = "winwincode/v1";
const MAX_PROTOCOL_REQUEST_BYTES: usize = 2 * 1024 * 1024;
const MAX_RELAY_STATE_BYTES: usize = 2_048;

/// Canonical adapter-to-browser-session composition used by the public server.
pub struct EnterpriseIdentityProtocolApplication {
    protocols: Arc<EnterpriseIdentityProtocolAdapter>,
    sessions: ExternalIdentitySessionIssuer,
}

impl EnterpriseIdentityProtocolApplication {
    #[must_use]
    pub fn new(
        protocols: Arc<EnterpriseIdentityProtocolAdapter>,
        sessions: Arc<SqliteAuthSessionManager>,
    ) -> Self {
        Self {
            protocols,
            sessions: ExternalIdentitySessionIssuer::new(sessions),
        }
    }

    fn authenticate_oidc(
        &self,
        raw_token: String,
    ) -> Result<ExternalIdentitySessionResult, EnterpriseIdentityTransportError> {
        let token = OidcIdToken::new(raw_token).map_err(EnterpriseIdentityTransportError::from)?;
        let outcome = self
            .protocols
            .authenticate_oidc(&token)
            .map_err(EnterpriseIdentityTransportError::from)?;
        self.sessions
            .issue(outcome)
            .map_err(|_| EnterpriseIdentityTransportError::unavailable())
    }

    fn authenticate_saml(
        &self,
        raw_response: Vec<u8>,
    ) -> Result<ExternalIdentitySessionResult, EnterpriseIdentityTransportError> {
        let response =
            SamlResponse::new(raw_response).map_err(EnterpriseIdentityTransportError::from)?;
        let outcome = self
            .protocols
            .authenticate_saml(&response)
            .map_err(EnterpriseIdentityTransportError::from)?;
        self.sessions
            .issue(outcome)
            .map_err(|_| EnterpriseIdentityTransportError::unavailable())
    }

    fn apply_scim(
        &self,
        raw_bearer: String,
        event: &ScimLifecycleEvent,
    ) -> Result<(), EnterpriseIdentityTransportError> {
        let bearer =
            ScimBearerToken::new(raw_bearer).map_err(EnterpriseIdentityTransportError::from)?;
        self.protocols
            .apply_scim(&bearer, event)
            .map(|_| ())
            .map_err(EnterpriseIdentityTransportError::from)
    }
}

impl fmt::Debug for EnterpriseIdentityProtocolApplication {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("EnterpriseIdentityProtocolApplication")
            .field("protocols", &"[REDACTED]")
            .field("sessions", &"[REDACTED]")
            .finish()
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum EnterpriseIdentityTransportErrorKind {
    InvalidRequest,
    AuthenticationRejected,
    Conflict,
    Unavailable,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct EnterpriseIdentityTransportError {
    kind: EnterpriseIdentityTransportErrorKind,
}

impl EnterpriseIdentityTransportError {
    const fn invalid_request() -> Self {
        Self {
            kind: EnterpriseIdentityTransportErrorKind::InvalidRequest,
        }
    }

    const fn unavailable() -> Self {
        Self {
            kind: EnterpriseIdentityTransportErrorKind::Unavailable,
        }
    }

    const fn status(self) -> StatusCode {
        match self.kind {
            EnterpriseIdentityTransportErrorKind::InvalidRequest => StatusCode::BAD_REQUEST,
            EnterpriseIdentityTransportErrorKind::AuthenticationRejected => {
                StatusCode::UNAUTHORIZED
            }
            EnterpriseIdentityTransportErrorKind::Conflict => StatusCode::CONFLICT,
            EnterpriseIdentityTransportErrorKind::Unavailable => StatusCode::SERVICE_UNAVAILABLE,
        }
    }

    const fn code(self) -> &'static str {
        match self.kind {
            EnterpriseIdentityTransportErrorKind::InvalidRequest => "INVALID_IDENTITY_REQUEST",
            EnterpriseIdentityTransportErrorKind::AuthenticationRejected => {
                "IDENTITY_AUTHENTICATION_REJECTED"
            }
            EnterpriseIdentityTransportErrorKind::Conflict => "IDENTITY_CALLBACK_CONFLICT",
            EnterpriseIdentityTransportErrorKind::Unavailable => {
                "IDENTITY_VERIFICATION_UNAVAILABLE"
            }
        }
    }

    const fn message(self) -> &'static str {
        match self.kind {
            EnterpriseIdentityTransportErrorKind::InvalidRequest => {
                "identity protocol request is invalid"
            }
            EnterpriseIdentityTransportErrorKind::AuthenticationRejected => {
                "identity protocol authentication was rejected"
            }
            EnterpriseIdentityTransportErrorKind::Conflict => {
                "identity protocol callback conflicts with durable state"
            }
            EnterpriseIdentityTransportErrorKind::Unavailable => {
                "identity protocol verification is unavailable"
            }
        }
    }
}

impl From<EnterpriseProtocolError> for EnterpriseIdentityTransportError {
    fn from(error: EnterpriseProtocolError) -> Self {
        let kind = match error.kind() {
            EnterpriseProtocolErrorKind::InvalidRequest => {
                EnterpriseIdentityTransportErrorKind::InvalidRequest
            }
            EnterpriseProtocolErrorKind::SignatureRejected
            | EnterpriseProtocolErrorKind::IssuerMismatch
            | EnterpriseProtocolErrorKind::AudienceMismatch
            | EnterpriseProtocolErrorKind::Expired
            | EnterpriseProtocolErrorKind::NotYetValid => {
                EnterpriseIdentityTransportErrorKind::AuthenticationRejected
            }
            EnterpriseProtocolErrorKind::ReplayConflict
            | EnterpriseProtocolErrorKind::OutOfOrder
            | EnterpriseProtocolErrorKind::SubjectBusy => {
                EnterpriseIdentityTransportErrorKind::Conflict
            }
            EnterpriseProtocolErrorKind::VerificationUnavailable
            | EnterpriseProtocolErrorKind::LifecycleRejected
            | EnterpriseProtocolErrorKind::StorageUnavailable
            | EnterpriseProtocolErrorKind::ClockUnavailable => {
                EnterpriseIdentityTransportErrorKind::Unavailable
            }
        };
        Self { kind }
    }
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct OidcCallbackRequest {
    schema_version: String,
    id_token: String,
}

/// Builds the three protocol routes on the server's single public origin.
pub(crate) fn router(application: Arc<EnterpriseIdentityProtocolApplication>) -> Router {
    Router::new()
        .route("/api/v1/auth/oidc/callback", post(oidc_callback))
        .route("/api/v1/auth/saml/acs", post(saml_acs))
        .route("/api/v1/scim/events", post(scim_event))
        .with_state(application)
}

async fn oidc_callback(
    State(application): State<Arc<EnterpriseIdentityProtocolApplication>>,
    request: Request<Body>,
) -> Response {
    if request.uri().query().is_some() {
        return transport_error(EnterpriseIdentityTransportError::invalid_request());
    }
    let content_type = request
        .headers()
        .get(CONTENT_TYPE)
        .and_then(|value| value.to_str().ok());
    let is_json = content_type.is_some_and(canonical_json_content_type);
    let is_form = content_type.is_some_and(canonical_form_content_type);
    if !is_json && !is_form {
        return transport_error(EnterpriseIdentityTransportError::invalid_request());
    }
    let Ok(body) = to_bytes(request.into_body(), MAX_PROTOCOL_REQUEST_BYTES).await else {
        return transport_error(EnterpriseIdentityTransportError::invalid_request());
    };
    if body.is_empty() {
        return transport_error(EnterpriseIdentityTransportError::invalid_request());
    }
    let callback = if is_json {
        serde_json::from_slice::<OidcCallbackRequest>(&body)
            .ok()
            .filter(|callback| callback.schema_version == SUPPORTED_SCHEMA_VERSION)
    } else {
        parse_oidc_form(&body)
    };
    let Some(callback) = callback else {
        return transport_error(EnterpriseIdentityTransportError::invalid_request());
    };
    match application.authenticate_oidc(callback.id_token) {
        Ok(session) => login_response(&session),
        Err(error) => transport_error(error),
    }
}

async fn saml_acs(
    State(application): State<Arc<EnterpriseIdentityProtocolApplication>>,
    request: Request<Body>,
) -> Response {
    if request.uri().query().is_some()
        || !request
            .headers()
            .get(CONTENT_TYPE)
            .and_then(|value| value.to_str().ok())
            .is_some_and(canonical_form_content_type)
    {
        return transport_error(EnterpriseIdentityTransportError::invalid_request());
    }
    let body = match to_bytes(request.into_body(), MAX_PROTOCOL_REQUEST_BYTES).await {
        Ok(body) if !body.is_empty() => body,
        _ => return transport_error(EnterpriseIdentityTransportError::invalid_request()),
    };
    let Some(encoded) = parse_saml_form(&body) else {
        return transport_error(EnterpriseIdentityTransportError::invalid_request());
    };
    let response = match STANDARD.decode(encoded) {
        Ok(response) if !response.is_empty() => response,
        _ => return transport_error(EnterpriseIdentityTransportError::invalid_request()),
    };
    match application.authenticate_saml(response) {
        Ok(session) => login_response(&session),
        Err(error) => transport_error(error),
    }
}

async fn scim_event(
    State(application): State<Arc<EnterpriseIdentityProtocolApplication>>,
    request: Request<Body>,
) -> Response {
    if request.uri().query().is_some()
        || request.headers().contains_key(COOKIE)
        || !request
            .headers()
            .get(CONTENT_TYPE)
            .and_then(|value| value.to_str().ok())
            .is_some_and(canonical_json_content_type)
    {
        return transport_error(EnterpriseIdentityTransportError::invalid_request());
    }
    let Some(bearer) = exact_bearer(request.headers()) else {
        return transport_error(EnterpriseIdentityTransportError {
            kind: EnterpriseIdentityTransportErrorKind::AuthenticationRejected,
        });
    };
    let body = match to_bytes(request.into_body(), MAX_PROTOCOL_REQUEST_BYTES).await {
        Ok(body) if !body.is_empty() => body,
        _ => return transport_error(EnterpriseIdentityTransportError::invalid_request()),
    };
    let Ok(event) = serde_json::from_slice::<ScimLifecycleEvent>(&body) else {
        return transport_error(EnterpriseIdentityTransportError::invalid_request());
    };
    match application.apply_scim(bearer, &event) {
        Ok(()) => {
            let mut response = StatusCode::NO_CONTENT.into_response();
            prevent_caching(&mut response);
            response
        }
        Err(error) => transport_error(error),
    }
}

fn login_response(session: &ExternalIdentitySessionResult) -> Response {
    let Ok(cookie) = session.set_cookie_header() else {
        return transport_error(EnterpriseIdentityTransportError::unavailable());
    };
    let Ok(response) = session.response() else {
        return transport_error(EnterpriseIdentityTransportError::unavailable());
    };
    let value = match response {
        Some(value) => match serde_json::to_value(value) {
            Ok(value) => value,
            Err(_) => return transport_error(EnterpriseIdentityTransportError::unavailable()),
        },
        None => json!({
            "schemaVersion": SUPPORTED_SCHEMA_VERSION,
            "kind": "external_identity_callback_replay"
        }),
    };
    let status = if cookie.is_some() {
        StatusCode::CREATED
    } else {
        StatusCode::OK
    };
    let mut response = (status, axum::Json(value)).into_response();
    prevent_caching(&mut response);
    if let Some(cookie) = cookie {
        let Ok(cookie) = HeaderValue::from_str(&cookie) else {
            return transport_error(EnterpriseIdentityTransportError::unavailable());
        };
        response.headers_mut().insert(SET_COOKIE, cookie);
    }
    response
}

fn transport_error(error: EnterpriseIdentityTransportError) -> Response {
    let mut response = (
        error.status(),
        axum::Json(json!({
            "schemaVersion": SUPPORTED_SCHEMA_VERSION,
            "error": {
                "code": error.code(),
                "message": error.message()
            }
        })),
    )
        .into_response();
    prevent_caching(&mut response);
    response
}

fn prevent_caching(response: &mut Response) {
    response
        .headers_mut()
        .insert(CACHE_CONTROL, HeaderValue::from_static("no-store"));
}

fn parse_oidc_form(body: &[u8]) -> Option<OidcCallbackRequest> {
    let fields = parse_form(body)?;
    let mut id_token = None;
    for (name, value) in fields {
        match name.as_slice() {
            b"id_token" if id_token.is_none() => {
                id_token = String::from_utf8(value).ok();
            }
            b"state" if value.len() <= MAX_RELAY_STATE_BYTES => {}
            _ => return None,
        }
    }
    Some(OidcCallbackRequest {
        schema_version: SUPPORTED_SCHEMA_VERSION.to_owned(),
        id_token: id_token?,
    })
}

fn parse_saml_form(body: &[u8]) -> Option<Vec<u8>> {
    let fields = parse_form(body)?;
    let mut response = None;
    for (name, value) in fields {
        match name.as_slice() {
            b"SAMLResponse" if response.is_none() => response = Some(value),
            b"RelayState" if value.len() <= MAX_RELAY_STATE_BYTES => {}
            _ => return None,
        }
    }
    response
}

fn parse_form(body: &[u8]) -> Option<Vec<(Vec<u8>, Vec<u8>)>> {
    if body.is_empty() || body.len() > MAX_PROTOCOL_REQUEST_BYTES {
        return None;
    }
    body.split(|byte| *byte == b'&')
        .map(|field| {
            let separator = field.iter().position(|byte| *byte == b'=')?;
            let name = percent_decode(&field[..separator])?;
            let value = percent_decode(&field[separator + 1..])?;
            Some((name, value))
        })
        .collect()
}

fn percent_decode(value: &[u8]) -> Option<Vec<u8>> {
    let mut output = Vec::with_capacity(value.len());
    let mut index = 0;
    while index < value.len() {
        match value[index] {
            b'+' => output.push(b' '),
            b'%' => {
                let high = *value.get(index + 1)?;
                let low = *value.get(index + 2)?;
                output.push(hex(high)?.checked_mul(16)?.checked_add(hex(low)?)?);
                index += 2;
            }
            byte if !byte.is_ascii_control() => output.push(byte),
            _ => return None,
        }
        index += 1;
    }
    Some(output)
}

const fn hex(value: u8) -> Option<u8> {
    match value {
        b'0'..=b'9' => Some(value - b'0'),
        b'a'..=b'f' => Some(value - b'a' + 10),
        b'A'..=b'F' => Some(value - b'A' + 10),
        _ => None,
    }
}

fn exact_bearer(headers: &HeaderMap) -> Option<String> {
    let value = headers.get(AUTHORIZATION)?.to_str().ok()?;
    let bearer = value.strip_prefix("Bearer ")?;
    if bearer.is_empty()
        || bearer.len() > MAX_PROTOCOL_REQUEST_BYTES
        || bearer.trim() != bearer
        || bearer.chars().any(char::is_control)
    {
        return None;
    }
    Some(bearer.to_owned())
}

fn canonical_json_content_type(value: &str) -> bool {
    value.eq_ignore_ascii_case("application/json")
}

fn canonical_form_content_type(value: &str) -> bool {
    value.eq_ignore_ascii_case("application/x-www-form-urlencoded")
}
