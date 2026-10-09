// SPDX-License-Identifier: Apache-2.0

//! Device-local Sign in with `ChatGPT`, using the official open-source public-client flow.

#[cfg(test)]
#[path = "chatgpt_oauth_diagnostics.test.rs"]
mod diagnostics_tests;

use std::{
    collections::BTreeMap,
    io::{Read, Write},
    net::{TcpListener, TcpStream},
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

use base64::{Engine as _, engine::general_purpose::URL_SAFE_NO_PAD};
use jsonwebtoken::{Algorithm, DecodingKey, Validation, decode, decode_header, jwk::JwkSet};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use url::Url;

use crate::{ResolvedSecret, device_store::DeviceProviderError};

pub(crate) const ENDPOINT: &str = "https://api.openai.com/v1/responses";
const ISSUER: &str = "https://auth.openai.com";
const AUTHORIZE: &str = "https://auth.openai.com/api/accounts/authorize";
const TOKEN: &str = "https://auth.openai.com/api/accounts/oauth/token";
const JWKS: &str = "https://auth.openai.com/.well-known/jwks.json";
const RESOURCE: &str = "https://api.openai.com/v1";
const SCOPES: &str =
    "openid profile email offline_access resource.invoke chatgpt.tokens.use.direct";
const DIRECT_SCOPE: &str = "chatgpt.tokens.use.direct";
const CALLBACK: &str = "/auth/callback";
const LOGIN_TIMEOUT: Duration = Duration::from_mins(3);

/// Only the protected Device database serializes this record.
#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Credentials {
    pub(crate) client_id: String,
    pub(crate) subject: String,
    access_token: String,
    refresh_token: String,
    id_token: String,
    scope: String,
    expires_at: u64,
}

impl Drop for Credentials {
    fn drop(&mut self) {
        clear(&mut self.access_token);
        clear(&mut self.refresh_token);
        clear(&mut self.id_token);
    }
}

impl Credentials {
    pub(crate) fn needs_refresh(&self) -> Result<bool, DeviceProviderError> {
        Ok(self.expires_at <= now()?.saturating_add(60))
    }

    pub(crate) fn secret(&self) -> Result<ResolvedSecret, DeviceProviderError> {
        if !valid_client(&self.client_id)
            || !has_permission(&self.scope)
            || !valid_token(&self.access_token)
            || self.subject.is_empty()
            || self.needs_refresh()?
        {
            return Err(DeviceProviderError);
        }
        ResolvedSecret::from_bytes(self.access_token.as_bytes().to_vec())
            .map_err(|_| DeviceProviderError)
    }

    pub(crate) fn refresh(&mut self) -> Result<(), DeviceProviderError> {
        if !valid_client(&self.client_id) || !valid_token(&self.refresh_token) {
            return Err(DeviceProviderError);
        }
        let agent = agent();
        self.refresh_with(
            || load_jwks(&agent),
            |client_id, refresh_token| {
                token_request(
                    &agent,
                    &[
                        ("grant_type", "refresh_token"),
                        ("client_id", client_id),
                        ("refresh_token", refresh_token),
                        ("resource", RESOURCE),
                    ],
                )
            },
        )
    }

    fn refresh_with(
        &mut self,
        load_keys: impl FnOnce() -> Result<JwkSet, DeviceProviderError>,
        exchange: impl FnOnce(&str, &str) -> Result<TokenResponse, DeviceProviderError>,
    ) -> Result<(), DeviceProviderError> {
        // Discover identity keys before sending a grant that can rotate the refresh token.
        let keys = load_keys()?;
        let response = exchange(&self.client_id, &self.refresh_token)?;
        self.accept_refresh(&response, Some(&keys))
    }

    fn accept_refresh(
        &mut self,
        response: &TokenResponse,
        keys: Option<&JwkSet>,
    ) -> Result<(), DeviceProviderError> {
        if !response.token_type.eq_ignore_ascii_case("bearer")
            || !valid_token(&response.access_token)
        {
            return Err(DeviceProviderError);
        }
        let scope = response.scope.as_deref().unwrap_or(&self.scope);
        if !has_permission(scope) {
            return Err(DeviceProviderError);
        }
        // Refresh responses may omit the ID token. A supplied identity must remain bound.
        if let Some(id) = response.id_token.as_deref() {
            let claims =
                validate_identity(id, &self.client_id, None, keys.ok_or(DeviceProviderError)?)?;
            if claims.sub != self.subject {
                return Err(DeviceProviderError);
            }
        }
        let expires_at = expiry(response.expires_in)?;
        if response
            .refresh_token
            .as_ref()
            .is_some_and(|token| !valid_token(token))
        {
            return Err(DeviceProviderError);
        }
        clear(&mut self.access_token);
        self.access_token.clone_from(&response.access_token);
        self.scope = scope.to_owned();
        self.expires_at = expires_at;
        if let Some(refresh) = response.refresh_token.as_ref() {
            clear(&mut self.refresh_token);
            self.refresh_token.clone_from(refresh);
        }
        if let Some(id) = response.id_token.as_ref() {
            clear(&mut self.id_token);
            self.id_token.clone_from(id);
        }
        Ok(())
    }
}

fn clear(value: &mut String) {
    let mut bytes = std::mem::take(value).into_bytes();
    bytes.fill(0);
}

fn valid_token(value: &str) -> bool {
    !value.is_empty() && value.len() <= 32_768 && !value.chars().any(char::is_control)
}

fn valid_client(value: &str) -> bool {
    value.starts_with("oaiapp_")
        && value.len() <= 200
        && value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || byte == b'_' || byte == b'-')
}

fn has_permission(scope: &str) -> bool {
    scope
        .split_ascii_whitespace()
        .any(|value| value == DIRECT_SCOPE)
        && scope
            .split_ascii_whitespace()
            .any(|value| value == "resource.invoke")
}

fn now() -> Result<u64, DeviceProviderError> {
    Ok(SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_err(|_| DeviceProviderError)?
        .as_secs())
}

fn expiry(expires_in: u64) -> Result<u64, DeviceProviderError> {
    if !(61..=604_800).contains(&expires_in) {
        return Err(DeviceProviderError);
    }
    now()?.checked_add(expires_in).ok_or(DeviceProviderError)
}

fn random() -> Result<String, DeviceProviderError> {
    let mut bytes = [0u8; 32];
    getrandom::fill(&mut bytes).map_err(|_| DeviceProviderError)?;
    let value = URL_SAFE_NO_PAD.encode(bytes);
    bytes.fill(0);
    Ok(value)
}

fn agent() -> ureq::Agent {
    ureq::Agent::config_builder()
        .timeout_global(Some(Duration::from_secs(30)))
        .http_status_as_error(false)
        .max_redirects(0)
        .build()
        .into()
}

#[derive(Deserialize)]
struct TokenResponse {
    access_token: String,
    refresh_token: Option<String>,
    id_token: Option<String>,
    token_type: String,
    scope: Option<String>,
    expires_in: u64,
}

impl Drop for TokenResponse {
    fn drop(&mut self) {
        clear(&mut self.access_token);
        if let Some(value) = self.refresh_token.as_mut() {
            clear(value);
        }
        if let Some(value) = self.id_token.as_mut() {
            clear(value);
        }
    }
}

fn token_request(
    agent: &ureq::Agent,
    fields: &[(&str, &str)],
) -> Result<TokenResponse, DeviceProviderError> {
    // A grant can rotate credentials. An unknown response must not replay it.
    let response = winwincode_network::http::execute_http_once(
        agent,
        |agent| agent.post(TOKEN).send_form(fields.iter().copied()),
        524_288,
        Duration::from_secs(30),
        || true,
    )
    .map_err(|_| DeviceProviderError)?;
    let response: TokenResponse = read_json(response)?;
    if !response.token_type.eq_ignore_ascii_case("bearer") || !valid_token(&response.access_token) {
        return Err(DeviceProviderError);
    }
    Ok(response)
}

fn read_json<T: serde::de::DeserializeOwned>(
    response: ureq::http::Response<Vec<u8>>,
) -> Result<T, DeviceProviderError> {
    let success = response.status().is_success();
    let mut bytes = response.into_body();
    let parsed = if success && bytes.len() <= 524_288 {
        serde_json::from_slice(&bytes).map_err(|_| DeviceProviderError)
    } else {
        Err(DeviceProviderError)
    };
    bytes.fill(0);
    parsed
}

fn load_jwks(agent: &ureq::Agent) -> Result<JwkSet, DeviceProviderError> {
    read_json(
        winwincode_network::http::execute_http(
            agent,
            |agent| agent.get(JWKS).call(),
            524_288,
            winwincode_network::Replay::ReplayExact,
            Duration::from_secs(30),
            || true,
        )
        .map_err(|_| DeviceProviderError)?,
    )
}

#[derive(Deserialize)]
struct Identity {
    sub: String,
    iat: u64,
    nonce: Option<String>,
}

fn validate_identity(
    token: &str,
    client: &str,
    nonce: Option<&str>,
    keys: &JwkSet,
) -> Result<Identity, DeviceProviderError> {
    let header = decode_header(token).map_err(|_| DeviceProviderError)?;
    if header.alg != Algorithm::RS256 {
        return Err(DeviceProviderError);
    }
    let kid = header.kid.ok_or(DeviceProviderError)?;
    let jwk = keys.find(&kid).ok_or(DeviceProviderError)?;
    let key = DecodingKey::from_jwk(jwk).map_err(|_| DeviceProviderError)?;
    let mut validation = Validation::new(Algorithm::RS256);
    validation.set_issuer(&[ISSUER]);
    validation.set_audience(&[client]);
    validation.set_required_spec_claims(&["sub", "exp", "iat"]);
    validation.leeway = 5;
    validation.validate_nbf = true;
    let claims = decode::<Identity>(token, &key, &validation)
        .map_err(|_| DeviceProviderError)?
        .claims;
    if claims.sub.is_empty()
        || claims.sub.len() > 200
        || claims.sub.chars().any(char::is_control)
        || claims.iat > now()?.saturating_add(5)
        || nonce.is_some_and(|expected| claims.nonce.as_deref() != Some(expected))
    {
        return Err(DeviceProviderError);
    }
    Ok(claims)
}

struct Pending {
    listener: TcpListener,
    redirect: String,
    verifier: String,
    state: String,
    nonce: String,
    client_id: Option<String>,
}

impl Drop for Pending {
    fn drop(&mut self) {
        clear(&mut self.verifier);
    }
}

enum Callback {
    Code { code: String, client_id: String },
    Denied,
    Ignore,
}

impl Pending {
    fn new(previous: Option<&Credentials>) -> Result<Self, DeviceProviderError> {
        let listener = TcpListener::bind("127.0.0.1:0")?;
        listener.set_nonblocking(true)?;
        let redirect = format!(
            "http://127.0.0.1:{}{CALLBACK}",
            listener.local_addr()?.port()
        );
        Ok(Self {
            listener,
            redirect,
            verifier: random()?,
            state: random()?,
            nonce: random()?,
            client_id: previous.map(|record| record.client_id.clone()),
        })
    }

    fn url(
        &self,
        host_id: &str,
        previous: Option<&Credentials>,
    ) -> Result<Url, DeviceProviderError> {
        let mut url = Url::parse(AUTHORIZE).map_err(|_| DeviceProviderError)?;
        let mut query = url.query_pairs_mut();
        query.extend_pairs([
            (
                "client_id",
                self.client_id.as_deref().unwrap_or("dynamic_agent_client"),
            ),
            ("ext_agent_host_id", host_id),
            ("response_type", "code"),
            ("redirect_uri", &self.redirect),
            ("scope", SCOPES),
            ("resource", RESOURCE),
            ("state", &self.state),
            ("nonce", &self.nonce),
            ("code_challenge_method", "S256"),
            (
                "code_challenge",
                &URL_SAFE_NO_PAD.encode(Sha256::digest(self.verifier.as_bytes())),
            ),
        ]);
        if let Some(previous) = previous {
            query.append_pair("id_token_hint", &previous.id_token);
        } else {
            query.append_pair("agent_name_hint", "WinWinCode");
        }
        drop(query);
        Ok(url)
    }

    fn callback(&self, target: &str) -> Callback {
        if !target.starts_with('/') || target.starts_with("//") {
            return Callback::Ignore;
        }
        let Ok(url) = Url::parse(&format!("http://127.0.0.1{target}")) else {
            return Callback::Ignore;
        };
        if url.path() != CALLBACK || url.fragment().is_some() {
            return Callback::Ignore;
        }
        let mut params = BTreeMap::new();
        for (key, value) in url.query_pairs() {
            if params
                .insert(key.into_owned(), value.into_owned())
                .is_some()
            {
                return Callback::Ignore;
            }
        }
        if params.get("state") != Some(&self.state) {
            return Callback::Ignore;
        }
        if params.contains_key("error") {
            return Callback::Denied;
        }
        let Some(code) = params.get("code").filter(|code| valid_token(code)) else {
            return Callback::Denied;
        };
        let client = match (&self.client_id, params.get("client_id")) {
            (Some(expected), Some(actual)) if expected != actual => return Callback::Denied,
            (Some(expected), _) => expected,
            (None, Some(issued)) if valid_client(issued) => issued,
            _ => return Callback::Denied,
        };
        Callback::Code {
            code: code.clone(),
            client_id: client.clone(),
        }
    }

    fn wait(&self) -> Result<(String, String, TcpStream), DeviceProviderError> {
        let deadline = Instant::now() + LOGIN_TIMEOUT;
        while Instant::now() < deadline {
            match self.listener.accept() {
                Ok((mut stream, peer)) => {
                    if !peer.ip().is_loopback() {
                        continue;
                    }
                    stream.set_read_timeout(Some(Duration::from_millis(500)))?;
                    stream.set_write_timeout(Some(Duration::from_millis(500)))?;
                    let callback = self.read_callback(&mut stream).unwrap_or(Callback::Ignore);
                    match callback {
                        Callback::Code { code, client_id } => return Ok((code, client_id, stream)),
                        Callback::Denied => {
                            respond(&mut stream, false);
                            return Err(DeviceProviderError);
                        }
                        Callback::Ignore => respond(&mut stream, false),
                    }
                }
                Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                    std::thread::sleep(Duration::from_millis(50));
                }
                Err(error) => return Err(error.into()),
            }
        }
        Err(DeviceProviderError)
    }

    fn read_callback(&self, stream: &mut TcpStream) -> Result<Callback, DeviceProviderError> {
        stream.set_nonblocking(false)?;
        stream.set_read_timeout(Some(Duration::from_millis(500)))?;
        stream.set_write_timeout(Some(Duration::from_millis(500)))?;
        let deadline = Instant::now() + Duration::from_secs(2);
        let mut bytes = Vec::new();
        let mut buffer = [0u8; 1024];
        while bytes.len() <= 16_384 {
            if Instant::now() >= deadline {
                return Err(DeviceProviderError);
            }
            let count = stream.read(&mut buffer)?;
            if count == 0 {
                return Err(DeviceProviderError);
            }
            bytes.extend_from_slice(&buffer[..count]);
            if bytes.windows(4).any(|window| window == b"\r\n\r\n") {
                break;
            }
        }
        if bytes.len() > 16_384 {
            return Err(DeviceProviderError);
        }
        let request = std::str::from_utf8(&bytes).map_err(|_| DeviceProviderError)?;
        let mut lines = request.split("\r\n");
        let mut first = lines
            .next()
            .ok_or(DeviceProviderError)?
            .split_ascii_whitespace();
        if first.next() != Some("GET") {
            return Err(DeviceProviderError);
        }
        let target = first.next().ok_or(DeviceProviderError)?;
        if first.next() != Some("HTTP/1.1") || first.next().is_some() {
            return Err(DeviceProviderError);
        }
        let expected_host = format!("127.0.0.1:{}", self.listener.local_addr()?.port());
        let hosts: Vec<_> = lines
            .filter_map(|line| line.split_once(':'))
            .filter(|(key, _)| key.eq_ignore_ascii_case("host"))
            .collect();
        if hosts.len() != 1 || hosts[0].1.trim() != expected_host {
            return Err(DeviceProviderError);
        }
        let result = self.callback(target);
        bytes.fill(0);
        Ok(result)
    }
}

fn respond(stream: &mut TcpStream, success: bool) {
    let body = if success {
        "WinWinCode authorization completed. Return to WinWinCode."
    } else {
        "WinWinCode authorization was not completed. Return to WinWinCode and retry."
    };
    let _ = write!(
        stream,
        "HTTP/1.1 200 OK\r\nContent-Type: text/plain; charset=utf-8\r\nCache-Control: no-store\r\nReferrer-Policy: no-referrer\r\nContent-Security-Policy: default-src 'none'\r\nConnection: close\r\nContent-Length: {}\r\n\r\n{body}",
        body.len()
    );
}

pub(crate) fn authorize(
    host_id: &str,
    previous: Option<&Credentials>,
) -> Result<Credentials, DeviceProviderError> {
    let pending = Pending::new(previous)?;
    let url = pending.url(host_id, previous)?;
    webbrowser::open(url.as_str()).map_err(|_| DeviceProviderError)?;
    let (mut code, client_id, mut stream) = pending.wait()?;
    let agent = agent();
    let result = (|| {
        let response = token_request(
            &agent,
            &[
                ("grant_type", "authorization_code"),
                ("client_id", &client_id),
                ("code", &code),
                ("code_verifier", &pending.verifier),
                ("redirect_uri", &pending.redirect),
                ("resource", RESOURCE),
            ],
        )?;
        credentials(
            &response,
            &client_id,
            &pending.nonce,
            &load_jwks(&agent)?,
            previous,
        )
    })();
    clear(&mut code);
    respond(&mut stream, result.is_ok());
    result
}

fn credentials(
    response: &TokenResponse,
    client: &str,
    nonce: &str,
    keys: &JwkSet,
    previous: Option<&Credentials>,
) -> Result<Credentials, DeviceProviderError> {
    if !valid_client(client)
        || !response.token_type.eq_ignore_ascii_case("bearer")
        || !valid_token(&response.access_token)
    {
        return Err(DeviceProviderError);
    }
    let id_token = response.id_token.as_ref().ok_or(DeviceProviderError)?;
    let claims = validate_identity(id_token, client, Some(nonce), keys)?;
    if previous.is_some_and(|record| record.subject != claims.sub || record.client_id != client) {
        return Err(DeviceProviderError);
    }
    let scope = response
        .scope
        .as_ref()
        .filter(|scope| has_permission(scope))
        .ok_or(DeviceProviderError)?;
    let refresh = response
        .refresh_token
        .as_ref()
        .filter(|token| valid_token(token))
        .ok_or(DeviceProviderError)?;
    Ok(Credentials {
        client_id: client.to_owned(),
        subject: claims.sub,
        access_token: response.access_token.clone(),
        refresh_token: refresh.clone(),
        id_token: id_token.clone(),
        scope: scope.clone(),
        expires_at: expiry(response.expires_in)?,
    })
}

/// Account-specific catalog. Only slugs cross the Device projection boundary.
pub(crate) fn models(record: &Credentials) -> Result<Vec<String>, DeviceProviderError> {
    #[derive(Deserialize)]
    struct Catalog {
        models: Vec<Model>,
    }
    #[derive(Deserialize)]
    struct Model {
        slug: String,
        visibility: String,
    }
    let catalog: Catalog = read_json(
        winwincode_network::http::execute_http(
            &agent(),
            |agent| {
                agent
                    .get("https://api.openai.com/v1/models")
                    .header("Authorization", &format!("Bearer {}", record.access_token))
                    .call()
            },
            524_288,
            winwincode_network::Replay::ReplayExact,
            Duration::from_secs(30),
            || true,
        )
        .map_err(|_| DeviceProviderError)?,
    )?;
    let models: Vec<_> = catalog
        .models
        .into_iter()
        .filter(|model| model.visibility == "list")
        .map(|model| model.slug)
        .filter(|slug| {
            !slug.is_empty()
                && slug.len() <= 128
                && !slug.chars().any(char::is_control)
                && slug.trim() == slug
        })
        .take(100)
        .collect();
    if models.is_empty() {
        return Err(DeviceProviderError);
    }
    Ok(models)
}

#[cfg(test)]
mod tests {
    use super::*;
    // Synthetic signing fixture, unrelated to any OpenAI credential.
    const TEST_KEY: &str = r"-----BEGIN RSA PRIVATE KEY-----
MIIEogIBAAKCAQEAjXqQw3jyx2AJ16NBOGq2a/YKudnHD9Kk+Oghcc+LlL6lM4sl
WzysF3+61nHEah4BY6/FikToRBW0XJkkdgPVtXmvRxL1RmNpRxlQQt9BgHrz8Yzx
97JsCXJoD4QQi4t63XSUOtKxbE7gFNgYqEmN6oS0LpQADespFZhE2dfxOe4u+c30
9nsuOKPuZyIgAmHQNMoXOWiBNP6HgrF0GPQwWpvsmJ5Pe7FpiaLfdgMI5+8IEWmf
hz24GWXwLAvJzHij7SUPB5bKyH11GY18YckMSPbVFrYrljiZftb4rZl7yIRkIU5h
QaI+FZ7FJj11up9d/4ieez4gYDsguoObv+FL1wIDAQABAoIBAEUmiZhJRxzlF5py
/J9XJUe6kD6Lr8zPj/vq9eHbaCVxU2zQj5c7HgA0Wb2UCMU3WkV/tcVh7cbNdwUl
gxk9wZh8XAwYu5LGZ0Atorm0xp7GOfKwCdqgNkcdyLgAvFeAerLAVu0zay39lXAK
uW6T2Q4uA6WaiDZFYSThcpGphwnhY5Y9xfwL4RA0/ZiAKJjnNSwujwZlWKhKzFGs
22nUkGDy1JJDoA8v0C2PLBvoCwJ2cPK9DdYiRM9OADGcq1KgBoZoYTlt9yQusNIr
EK7DcX/ZT/gXU0myyzMESonX7ikCLNlOI6qs/TnLmkELn4zrS012VPpOx/Nl6KkC
74032V0CgYEAxpO+RViwxNFPiGX/yX0mgKttgB/4+nFxSJoC4HHmMPMmvOSuGw9s
Z5jSh8qupDqql/ZJ5n0Hr80RwNJCtcJo3Ae12sHWuoe89tTAq4FZakD2gxJYD2AT
bouDn5i+JcTsjPwW5dzujsk6L6SQ66JS2vibw8Gm4b2OKlZGvGwK/t0CgYEAtmPy
01RcwhrXymP6ziqa8/RX2nehBxivhaQrjJQHufNYlEiymMVAkK9KIieslZleMUW3
dclkXaWai6bupQeZ8oNzXvNdSYiYp1cVGIBXNSfOc9XibL7ai1yx09wVK1JnA+BI
Z7Db+woJpNuWb+az1kzouBVBcRY3JUnfLdSdeEMCgYAkw1N1eS74sRt9UAzj25SW
O6aBEupAS2JCo6imrs+D/nAMhnWpMtjJ8SQA2cgtRWMx0Pnrfvg+VsPTo9mP1tcc
7RyAzGQZkmmsdMTau463OiGpMGs3JX1TeOa8VEXXrjZw/3apxZYwvxZTna7qwNmA
Coij3CUuI66/LcvYtFPwzQKBgANK1JRJ/o6ma2TE3z8fd8KVk4xnAmodYPW5m+ui
tGz/3oZ2tXqafOrfryfkrXHZ3eBn0ML4iq5CEgMZVU93TqkZBFkfbXypUZAbu07A
6lIUUN64aqjp3QoM90zKuTsZ9rAUOVpdz+q9KKVoLVQBxxdENiM0hwTikMZEihnK
r7j/AoGADdG4bJak8qYxRRL/IbgOv+bUCkz1QnEw87hxeiedk2OgImDsZs0aNn7e
NfMEoSeQL66X6RO7WoEsHWJlq4Skj3Ta/hs33g9Hm8rdZIJcI+MMpg9++ewG4ZW/
z3AxPR+IWK+k7GyAWgO6vtXmUm6is+DCthbCX8goI8kqDB8VUO0=
-----END RSA PRIVATE KEY-----
";
    const TEST_JWKS: &str = r#"{"keys": [{"kty": "RSA", "kid": "test-only", "alg": "RS256", "use": "sig", "n": "jXqQw3jyx2AJ16NBOGq2a_YKudnHD9Kk-Oghcc-LlL6lM4slWzysF3-61nHEah4BY6_FikToRBW0XJkkdgPVtXmvRxL1RmNpRxlQQt9BgHrz8Yzx97JsCXJoD4QQi4t63XSUOtKxbE7gFNgYqEmN6oS0LpQADespFZhE2dfxOe4u-c309nsuOKPuZyIgAmHQNMoXOWiBNP6HgrF0GPQwWpvsmJ5Pe7FpiaLfdgMI5-8IEWmfhz24GWXwLAvJzHij7SUPB5bKyH11GY18YckMSPbVFrYrljiZftb4rZl7yIRkIU5hQaI-FZ7FJj11up9d_4ieez4gYDsguoObv-FL1w", "e": "AQAB"}]}"#;

    #[test]
    fn dynamic_registration_uses_pkce_and_a_bound_loopback() {
        let pending = Pending::new(None).unwrap();
        let url = pending.url("urn:uuid:host-test", None).unwrap();
        let params: BTreeMap<_, _> = url.query_pairs().into_owned().collect();
        assert_eq!(params["client_id"], "dynamic_agent_client");
        assert_eq!(params["agent_name_hint"], "WinWinCode");
        assert_eq!(params["redirect_uri"], pending.redirect);
        assert!(pending.redirect.starts_with("http://127.0.0.1:"));
        assert_eq!(
            params["code_challenge"],
            URL_SAFE_NO_PAD.encode(Sha256::digest(pending.verifier.as_bytes()))
        );
        assert_eq!(params["resource"], RESOURCE);
        assert_eq!(params["state"], pending.state);
        assert_eq!(params["nonce"], pending.nonce);
        assert!(has_permission(&params["scope"]));
    }

    #[test]
    fn callbacks_reject_missing_state_duplicate_fields_and_unissued_clients() {
        let pending = Pending::new(None).unwrap();
        assert!(matches!(
            pending.callback("/auth/callback?code=a&client_id=oaiapp_a"),
            Callback::Ignore
        ));
        assert!(matches!(
            pending.callback(&format!(
                "/auth/callback?state={}&state={}&code=a&client_id=oaiapp_a",
                pending.state, pending.state
            )),
            Callback::Ignore
        ));
        assert!(matches!(
            pending.callback(&format!(
                "/auth/callback?state={}&code=a&client_id=dynamic_agent_client",
                pending.state
            )),
            Callback::Denied
        ));
        assert!(matches!(
            pending.callback(&format!(
                "/auth/callback?state={}&error=access_denied&code=a",
                pending.state
            )),
            Callback::Denied
        ));
        assert!(
            matches!(pending.callback(&format!("/auth/callback?state={}&code=a&client_id=oaiapp_issued", pending.state)), Callback::Code { client_id, .. } if client_id == "oaiapp_issued")
        );
    }

    #[test]
    fn returning_authorization_keeps_the_registration_and_identity_hint() {
        let record = Credentials {
            client_id: "oaiapp_selected".into(),
            subject: "selected-subject".into(),
            access_token: "access".into(),
            refresh_token: "refresh".into(),
            id_token: "retained-id".into(),
            scope: SCOPES.into(),
            expires_at: now().unwrap() + 3600,
        };
        let pending = Pending::new(Some(&record)).unwrap();
        let params: BTreeMap<_, _> = pending
            .url("host", Some(&record))
            .unwrap()
            .query_pairs()
            .into_owned()
            .collect();
        assert_eq!(params["client_id"], "oaiapp_selected");
        assert_eq!(params["id_token_hint"], "retained-id");
        assert!(!params.contains_key("agent_name_hint"));
        assert!(matches!(
            pending.callback(&format!(
                "/auth/callback?state={}&code=a&client_id=oaiapp_other",
                pending.state
            )),
            Callback::Denied
        ));
        assert!(matches!(
            pending.callback(&format!("/auth/callback?state={}&code=a", pending.state)),
            Callback::Code { .. }
        ));
        assert!(record.secret().is_ok());
    }

    fn signed_response(claims: &serde_json::Value) -> TokenResponse {
        let mut header = jsonwebtoken::Header::new(Algorithm::RS256);
        header.kid = Some("test-only".into());
        let encoded: String = TEST_KEY
            .lines()
            .filter(|line| !line.starts_with("---"))
            .collect();
        let der = base64::engine::general_purpose::STANDARD
            .decode(encoded)
            .unwrap();
        let token = jsonwebtoken::encode(
            &header,
            claims,
            &jsonwebtoken::EncodingKey::from_rsa_der(&der),
        )
        .unwrap();
        TokenResponse {
            access_token: "first-access".into(),
            refresh_token: Some("first-refresh".into()),
            id_token: Some(token),
            token_type: "Bearer".into(),
            scope: Some(SCOPES.into()),
            expires_in: 3600,
        }
    }

    fn claims() -> serde_json::Value {
        serde_json::json!({"iss":ISSUER,"aud":"oaiapp_test","sub":"user-one","iat":now().unwrap(),"exp":now().unwrap()+3600,"nonce":"pending-nonce"})
    }

    #[test]
    fn verified_identity_rejects_signature_audience_issuer_nonce_and_expiry_drift() {
        let keys: JwkSet = serde_json::from_str(TEST_JWKS).unwrap();
        let response = signed_response(&claims());
        let id = response.id_token.as_ref().unwrap();
        assert!(validate_identity(id, "oaiapp_test", Some("pending-nonce"), &keys).is_ok());
        assert!(validate_identity(id, "oaiapp_other", Some("pending-nonce"), &keys).is_err());
        assert!(validate_identity(id, "oaiapp_test", Some("foreign-nonce"), &keys).is_err());
        let mut tampered = id.clone().into_bytes();
        let signature_start = tampered.iter().rposition(|byte| *byte == b'.').unwrap() + 1;
        tampered[signature_start] = if tampered[signature_start] == b'A' {
            b'B'
        } else {
            b'A'
        };
        assert!(
            validate_identity(
                &String::from_utf8(tampered).unwrap(),
                "oaiapp_test",
                Some("pending-nonce"),
                &keys
            )
            .is_err()
        );
        for (field, value) in [
            ("iss", serde_json::json!("https://other.example")),
            ("exp", serde_json::json!(now().unwrap() - 600)),
            ("iat", serde_json::json!(now().unwrap() + 600)),
        ] {
            let mut changed = claims();
            changed[field] = value;
            let response = signed_response(&changed);
            assert!(credentials(&response, "oaiapp_test", "pending-nonce", &keys, None).is_err());
        }
    }

    #[test]
    fn unavailable_identity_keys_do_not_consume_a_rotating_refresh_token() {
        let keys: JwkSet = serde_json::from_str(TEST_JWKS).unwrap();
        let response = signed_response(&claims());
        let mut record =
            credentials(&response, "oaiapp_test", "pending-nonce", &keys, None).unwrap();
        let consumed = std::cell::Cell::new(false);
        assert!(
            record
                .refresh_with(
                    || Err(DeviceProviderError),
                    |_, _| {
                        consumed.set(true);
                        Ok(signed_response(&claims()))
                    }
                )
                .is_err()
        );
        assert!(
            !consumed.get(),
            "key discovery must succeed before the rotating grant is sent"
        );
    }

    #[test]
    fn authorization_and_refresh_keep_identity_permissions_and_rotated_tokens() {
        let keys: JwkSet = serde_json::from_str(TEST_JWKS).unwrap();
        let mut response = signed_response(&claims());
        let mut record =
            credentials(&response, "oaiapp_test", "pending-nonce", &keys, None).unwrap();
        response.scope = Some("openid email profile".into());
        assert!(credentials(&response, "oaiapp_test", "pending-nonce", &keys, None).is_err());
        response.scope = None;
        response.access_token = "rotated-access".into();
        response.refresh_token = Some("rotated-refresh".into());
        response.id_token = None;
        record.accept_refresh(&response, None).unwrap();
        assert_eq!(record.secret().unwrap().expose(), b"rotated-access");
        assert_eq!(record.refresh_token, "rotated-refresh");
        let mut changed = claims();
        changed["sub"] = serde_json::json!("user-two");
        let changed = signed_response(&changed);
        assert!(
            credentials(
                &changed,
                "oaiapp_test",
                "pending-nonce",
                &keys,
                Some(&record)
            )
            .is_err()
        );
        assert!(record.accept_refresh(&changed, Some(&keys)).is_err());
        assert_eq!(record.secret().unwrap().expose(), b"rotated-access");
        response.scope = Some("openid".into());
        assert!(record.accept_refresh(&response, None).is_err());
        assert_eq!(record.refresh_token, "rotated-refresh");
    }

    #[test]
    fn loopback_callback_requires_the_expected_host_and_returns_no_code_in_body() {
        let pending = Pending::new(None).unwrap();
        let address = pending.listener.local_addr().unwrap();
        let target = format!(
            "/auth/callback?state={}&code=private-code&client_id=oaiapp_test",
            pending.state
        );
        let mut client = TcpStream::connect(address).unwrap();
        write!(
            client,
            "GET {target} HTTP/1.1\r\nHost: attacker.example\r\n\r\n"
        )
        .unwrap();
        let (mut accepted, _) = pending.listener.accept().unwrap();
        assert!(pending.read_callback(&mut accepted).is_err());
        drop(accepted);
        drop(client);
        let mut client = TcpStream::connect(address).unwrap();
        write!(client, "GET {target} HTTP/1.1\r\nHost: {address}\r\n\r\n").unwrap();
        let (mut accepted, _) = pending.listener.accept().unwrap();
        assert!(matches!(
            pending.read_callback(&mut accepted).unwrap(),
            Callback::Code { .. }
        ));
        respond(&mut accepted, true);
        drop(accepted);
        let mut body = String::new();
        client.read_to_string(&mut body).unwrap();
        assert!(body.contains("Cache-Control: no-store"));
        assert!(!body.contains("private-code"));
        assert!(!body.contains(&pending.state));
    }
}
