// SPDX-License-Identifier: Apache-2.0

use std::fmt;
use std::path::{Path, PathBuf};
use std::str::FromStr;
use std::time::Duration;

use base64::Engine as _;
use base64::engine::general_purpose::STANDARD_NO_PAD as BASE64;
use ed25519_dalek::{Signer as _, SigningKey};
use rusqlite::{OptionalExtension as _, params};
pub use winwincode_api::generated::DeviceBindResponse as DeviceBindOutcome;
use winwincode_api::generated::{
    DeviceBindRequest, DeviceBindingIntent, DeviceChallengeRequest, DeviceChallengeResponse,
};

use crate::daemon::{DeviceAuthRejection, ExchangeTransportError};
use crate::identity::{DeviceIdentitySeed, generate_prefixed_id};
use crate::store::{DeviceStore, DeviceStoreError, sql_error};
use crate::{ensure_device_identity, load_device_identity};

const CHALLENGE_PATH: &str = "/internal/v1/device/challenge";
const BIND_PATH: &str = "/internal/v1/device/bind";
pub(crate) const DEVICE_REVOKED_REASON: &str = "DEVICE_REVOKED";

enum BindingRequestError {
    PairingRejected,
    Transport(ExchangeTransportError),
}

#[derive(Clone)]
pub struct DeviceRemoteAuth {
    server_url: String,
    signing_key: SigningKey,
    client_node_id: String,
    ca_roots: Vec<Vec<u8>>,
}

impl fmt::Debug for DeviceRemoteAuth {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("DeviceRemoteAuth")
            .field("server_url", &self.server_url)
            .field("client_node_id", &self.client_node_id)
            .finish_non_exhaustive()
    }
}

#[derive(Clone)]
pub struct DeviceBindConfig {
    pub data_directory: PathBuf,
    pub server_url: Option<String>,
    pub pairing_token: Option<String>,
    pub ca_file: Option<PathBuf>,
    pub seed: DeviceIdentitySeed,
}

pub struct RemoteBinding {
    pub server_url: String,
    pub request_id: String,
    pub failure_reason: Option<String>,
    auth: DeviceRemoteAuth,
    intent_json: String,
    pairing_token: Option<String>,
    receipt: Option<DeviceBindOutcome>,
}

impl RemoteBinding {
    #[must_use]
    pub fn auth(&self) -> DeviceRemoteAuth {
        self.auth.clone()
    }

    #[must_use]
    pub fn is_bound(&self) -> bool {
        self.receipt.is_some()
    }
}

impl DeviceRemoteAuth {
    #[must_use]
    pub fn server_url(&self) -> &str {
        &self.server_url
    }

    #[must_use]
    pub fn ca_roots(&self) -> &[Vec<u8>] {
        &self.ca_roots
    }

    #[must_use]
    pub fn with_ca_roots(mut self, roots: Vec<Vec<u8>>) -> Self {
        self.ca_roots = roots;
        self
    }

    pub(crate) fn post_json(
        &self,
        endpoint: &str,
        body: &[u8],
        timeout: Duration,
        can_start: &(dyn Fn() -> bool + Sync),
    ) -> Result<Vec<u8>, ExchangeTransportError> {
        crate::http::validate_endpoint(endpoint)?;
        let uri = http::Uri::from_str(endpoint).map_err(|_| crate::http::invalid_endpoint())?;
        let origin = format!(
            "{}://{}",
            uri.scheme_str().unwrap_or_default(),
            uri.authority().expect("validated authority")
        );
        if origin != self.server_url {
            return Err(crate::http::invalid_endpoint());
        }
        let path = uri
            .path_and_query()
            .map_or("/", http::uri::PathAndQuery::as_str);
        let mut refreshed = false;
        loop {
            let headers = self.authorization_headers("POST", path, body, timeout, can_start)?;
            let result = crate::http::post_json_with_options(
                endpoint,
                body,
                timeout,
                can_start,
                &self.ca_roots,
                &headers,
            );
            if matches!(&result, Err(error) if error.network_failure().http_status == Some(401))
                && !refreshed
            {
                refreshed = true;
            } else {
                return result;
            }
        }
    }

    fn post_binding(&self, body: &[u8]) -> Result<Vec<u8>, BindingRequestError> {
        let timeout = Duration::from_secs(10);
        let mut refreshed = false;
        loop {
            let headers = self
                .authorization_headers("POST", BIND_PATH, body, timeout, &|| true)
                .map_err(BindingRequestError::Transport)?;
            let result = crate::http::post_json_with_options(
                &format!("{}{BIND_PATH}", self.server_url),
                body,
                timeout,
                &|| true,
                &self.ca_roots,
                &headers,
            );
            if let Err(error) = &result {
                if error.device_auth_rejection() == Some(DeviceAuthRejection::PairingRejected) {
                    return Err(BindingRequestError::PairingRejected);
                }
                if error.network_failure().http_status == Some(401) && !refreshed {
                    refreshed = true;
                    continue;
                }
            }
            return result.map_err(BindingRequestError::Transport);
        }
    }

    pub fn authorization_headers(
        &self,
        method: &str,
        path: &str,
        body: &[u8],
        timeout: Duration,
        can_start: &(dyn Fn() -> bool + Sync),
    ) -> Result<Vec<(String, String)>, ExchangeTransportError> {
        let public_key = BASE64.encode(self.signing_key.verifying_key().as_bytes());
        let request: DeviceChallengeRequest = serde_json::from_value(serde_json::json!({
            "schemaVersion": "winwincode/v1", "publicKey": public_key,
        }))
        .map_err(|_| crate::http::invalid_endpoint())?;
        let request = serde_json::to_vec(&request).map_err(|_| crate::http::invalid_endpoint())?;
        let response = crate::http::post_json_with_options(
            &format!("{}{CHALLENGE_PATH}", self.server_url),
            &request,
            timeout,
            can_start,
            &self.ca_roots,
            &[],
        )?;
        let challenge: DeviceChallengeResponse =
            serde_json::from_slice(&response).map_err(|_| crate::http::invalid_endpoint())?;
        let bytes = winwincode_client_port::device_auth::signing_bytes(
            &challenge.challenge_id,
            &challenge.challenge,
            method,
            path,
            &self.client_node_id,
            body,
        );
        let mut headers = vec![
            ("x-winwincode-device-public-key".to_owned(), public_key),
            (
                "x-winwincode-device-challenge-id".to_owned(),
                challenge.challenge_id,
            ),
            (
                "x-winwincode-device-signature".to_owned(),
                BASE64.encode(self.signing_key.sign(&bytes).to_bytes()),
            ),
        ];
        if !self.client_node_id.is_empty() {
            headers.push((
                "x-winwincode-client-node-id".to_owned(),
                self.client_node_id.clone(),
            ));
        }
        Ok(headers)
    }
}

pub fn load_ca_roots(path: Option<&Path>) -> Result<Vec<Vec<u8>>, DeviceStoreError> {
    let Some(path) = path else {
        return Ok(Vec::new());
    };
    let pem = std::fs::read(path)
        .map_err(|_| DeviceStoreError::invalid("Device CA file cannot be read"))?;
    let mut roots = Vec::new();
    for item in ureq::tls::parse_pem(&pem) {
        match item.map_err(|_| DeviceStoreError::invalid("Device CA file contains invalid PEM"))? {
            ureq::tls::PemItem::Certificate(cert) => roots.push(cert.der().to_vec()),
            _ => {
                return Err(DeviceStoreError::invalid(
                    "Device CA file must contain certificates",
                ));
            }
        }
    }
    let mut parsed = rustls::RootCertStore::empty();
    for root in &roots {
        parsed
            .add(rustls::pki_types::CertificateDer::from(root.clone()))
            .map_err(|_| DeviceStoreError::invalid("Device CA certificate is invalid"))?;
    }
    if roots.is_empty() {
        return Err(DeviceStoreError::invalid(
            "Device CA file contains no certificates",
        ));
    }
    Ok(roots)
}

pub fn server_origin(url: &str) -> Result<String, DeviceStoreError> {
    let uri =
        http::Uri::from_str(url).map_err(|_| DeviceStoreError::invalid("Server URL is invalid"))?;
    crate::http::validate_endpoint(url)
        .map_err(|_| DeviceStoreError::invalid("Server URL requires HTTPS or loopback HTTP"))?;
    if uri.query().is_some() || !matches!(uri.path(), "" | "/" | "/internal/v1/client/exchange") {
        return Err(DeviceStoreError::invalid(
            "Server URL path must be / or /internal/v1/client/exchange",
        ));
    }
    Ok(format!(
        "{}://{}",
        uri.scheme_str().unwrap_or_default(),
        uri.authority().expect("validated authority")
    ))
}

pub fn load_remote_binding(store: &DeviceStore) -> Result<Option<RemoteBinding>, DeviceStoreError> {
    store.connection()?.query_row(
        "SELECT server_url, signing_key, request_id, intent_json, pairing_token, ca_roots_json, client_node_id, public_client_id, owner_user_id, bound_at, failure_reason FROM remote_device_binding WHERE singleton = 1",
        [], |row| {
            let secret: Vec<u8> = row.get(1)?;
            let secret: [u8; 32] = secret.try_into().map_err(|_| rusqlite::Error::InvalidQuery)?;
            let server_url: String = row.get(0)?;
            let request_id: String = row.get(2)?;
            let node: Option<String> = row.get(6)?;
            let receipt = if let Some(node) = &node {
                Some(serde_json::from_value::<DeviceBindOutcome>(serde_json::json!({ "schemaVersion": "winwincode/v1", "requestId": request_id, "clientNodeId": node, "publicClientId": row.get::<_, String>(7)?, "ownerUserId": row.get::<_, String>(8)?, "boundAt": row.get::<_, String>(9)? })).map_err(|_| rusqlite::Error::InvalidQuery)?)
            } else { None };
            let roots_json: String = row.get(5)?;
            let ca_roots = serde_json::from_str(&roots_json).map_err(|_| rusqlite::Error::InvalidQuery)?;
            Ok(RemoteBinding { server_url: server_url.clone(), request_id, failure_reason: row.get(10)?, auth: DeviceRemoteAuth { server_url, signing_key: SigningKey::from_bytes(&secret), client_node_id: node.unwrap_or_default(), ca_roots }, intent_json: row.get(3)?, pairing_token: row.get(4)?, receipt })
        }
    ).optional().map_err(sql_error)
}

pub fn record_remote_failure(store: &DeviceStore, reason: &str) -> Result<(), DeviceStoreError> {
    store
        .connection()?
        .execute(
            "UPDATE remote_device_binding SET failure_reason = ?1 WHERE singleton = 1",
            [reason],
        )
        .map_err(sql_error)?;
    Ok(())
}

pub(crate) fn retry_remote_authentication(store: &DeviceStore) -> Result<(), DeviceStoreError> {
    store.connection()?.execute(
        "UPDATE remote_device_binding SET failure_reason=NULL WHERE singleton=1 AND failure_reason IN (?1,?2)",
        ["Device 身份认证失败；服务已停止。", "Device 绑定已吊销；服务已停止。"],
    ).map_err(sql_error)?;
    Ok(())
}

fn remote_request_error(store: &DeviceStore, error: &ExchangeTransportError) -> DeviceStoreError {
    if error.device_auth_rejection() == Some(DeviceAuthRejection::Revoked) {
        let reason = "Device 绑定已吊销；服务已停止。";
        if let Err(storage_error) = record_remote_failure(store, DEVICE_REVOKED_REASON) {
            return storage_error;
        }
        DeviceStoreError::conflict(reason)
    } else {
        DeviceStoreError::adapter(error.to_string())
    }
}

#[allow(clippy::too_many_lines)]
pub fn bind_device(config: &DeviceBindConfig) -> Result<DeviceBindOutcome, DeviceStoreError> {
    let mut store = DeviceStore::open(&config.data_directory)?;
    retry_remote_authentication(&store)?;
    if load_device_identity(&store)?.is_none() {
        ensure_device_identity(
            &mut store,
            &config.seed,
            &crate::canonical_rfc3339(time::OffsetDateTime::now_utc()),
        )?;
    }
    if load_remote_binding(&store)?.is_none() {
        if load_device_identity(&store)?.is_some_and(|identity| identity.identity().is_enrolled()) {
            return Err(DeviceStoreError::conflict(
                "Device already has a local enrollment; use a separate data directory for remote binding",
            ));
        }
        let origin =
            server_origin(config.server_url.as_deref().ok_or_else(|| {
                DeviceStoreError::invalid("first binding requires a Server URL")
            })?)?;
        let token = config
            .pairing_token
            .as_deref()
            .filter(|token| !token.is_empty())
            .ok_or_else(|| DeviceStoreError::invalid("first binding requires a pairing token"))?;
        let roots = load_ca_roots(config.ca_file.as_deref())?;
        let mut secret = [0_u8; 32];
        getrandom::fill(&mut secret)
            .map_err(|_| DeviceStoreError::adapter("Device entropy unavailable"))?;
        let intent: DeviceBindingIntent = serde_json::from_value(serde_json::json!({
            "displayName": config.seed.display_name, "platform": config.seed.platform,
            "architecture": config.seed.architecture, "clientVersion": config.seed.client_version,
        }))
        .map_err(|_| DeviceStoreError::invalid("Device binding intent is invalid"))?;
        let intent_json = serde_json::to_string(&intent)
            .map_err(|_| DeviceStoreError::adapter("binding intent encoding failed"))?;
        let request_id = generate_prefixed_id("dbr_")?;
        store.connection()?.execute("INSERT OR IGNORE INTO remote_device_binding (singleton,server_url,signing_key,request_id,intent_json,pairing_token,ca_roots_json) VALUES (1,?1,?2,?3,?4,?5,?6)", params![origin, secret.as_slice(), request_id, intent_json, token, serde_json::to_string(&roots).map_err(|_| DeviceStoreError::adapter("CA roots encoding failed"))?]).map_err(sql_error)?;
    }
    if let Some(path) = config.ca_file.as_deref() {
        let roots = load_ca_roots(Some(path))?;
        let encoded = serde_json::to_string(&roots)
            .map_err(|_| DeviceStoreError::adapter("CA roots encoding failed"))?;
        store
            .connection()?
            .execute(
                "UPDATE remote_device_binding SET ca_roots_json = ?1 WHERE singleton = 1",
                [encoded],
            )
            .map_err(sql_error)?;
    }
    let binding = load_remote_binding(&store)?
        .ok_or_else(|| DeviceStoreError::adapter("persisted binding missing"))?;
    if let Some(url) = &config.server_url
        && server_origin(url)? != binding.server_url
    {
        return Err(DeviceStoreError::conflict(
            "Device binding belongs to another Server URL",
        ));
    }
    if let Some(reason) = &binding.failure_reason {
        return Err(DeviceStoreError::conflict(reason.clone()));
    }
    if let Some(receipt) = binding.receipt {
        return Ok(receipt);
    }
    let intent: serde_json::Value = serde_json::from_str(&binding.intent_json)
        .map_err(|_| DeviceStoreError::adapter("stored binding intent is invalid"))?;
    let mut request: DeviceBindRequest = serde_json::from_value(serde_json::json!({ "schemaVersion": "winwincode/v1", "requestId": binding.request_id, "pairingToken": binding.pairing_token, "publicKey": BASE64.encode(binding.auth.signing_key.verifying_key().as_bytes()), "intent": intent })).map_err(|_| DeviceStoreError::adapter("binding request encoding failed"))?;
    let mut replacement = config
        .pairing_token
        .as_deref()
        .filter(|token| !token.is_empty() && *token != request.pairing_token);
    let response = loop {
        let body = serde_json::to_vec(&request)
            .map_err(|_| DeviceStoreError::adapter("binding request encoding failed"))?;
        match binding.auth.post_binding(&body) {
            Ok(response) => break response,
            Err(BindingRequestError::PairingRejected) => {
                let token = replacement.take().ok_or_else(|| {
                    DeviceStoreError::conflict(
                        "配对令牌无效或已过期；请取得新令牌后在同一数据目录重试。",
                    )
                })?;
                let updated = store.connection()?.execute(
                    "UPDATE remote_device_binding SET pairing_token=?1 WHERE singleton=1 AND request_id=?2 AND pairing_token=?3 AND client_node_id IS NULL AND failure_reason IS NULL",
                    params![token, binding.request_id, request.pairing_token],
                ).map_err(sql_error)?;
                if updated != 1 {
                    return Err(DeviceStoreError::conflict(
                        "Device binding changed before pairing token replacement",
                    ));
                }
                request.pairing_token = token.to_owned();
            }
            Err(BindingRequestError::Transport(error)) => {
                return Err(remote_request_error(&store, &error));
            }
        }
    };
    let receipt: DeviceBindOutcome = serde_json::from_slice(&response)
        .map_err(|_| DeviceStoreError::adapter("Server binding response is invalid"))?;
    crate::identity::validate_issued_enrollment(&crate::IssuedEnrollment {
        client_node_id: receipt.client_node_id.0.clone(),
        public_client_id: receipt.public_client_id.0.clone(),
    })?;
    if receipt.request_id.0 != binding.request_id
        || receipt.client_node_id.0.is_empty()
        || receipt.public_client_id.0.is_empty()
    {
        return Err(DeviceStoreError::conflict(
            "Server binding response does not match pending request",
        ));
    }
    #[cfg(feature = "test-support")]
    if std::env::var("WWC_DEVICE_TEST_EXIT_BEFORE_BIND_RECEIPT_SAVE").as_deref() == Ok("1") {
        std::process::exit(86);
    }
    let transaction = store.connection_mut()?.transaction().map_err(sql_error)?;
    let adopted = transaction.execute("UPDATE device_identity SET client_node_id=?1,public_client_id=?2,revision=revision+1 WHERE client_node_id='' OR client_node_id=?1", params![receipt.client_node_id.0, receipt.public_client_id.0]).map_err(sql_error)?;
    if adopted != 1 {
        return Err(DeviceStoreError::conflict(
            "Device identity changed before binding adoption",
        ));
    }
    transaction.execute("UPDATE remote_device_binding SET client_node_id=?1,public_client_id=?2,owner_user_id=?3,bound_at=?4,pairing_token=NULL WHERE singleton=1", params![receipt.client_node_id.0, receipt.public_client_id.0, receipt.owner_user_id.0, receipt.bound_at.0]).map_err(sql_error)?;
    transaction.commit().map_err(sql_error)?;
    Ok(receipt)
}

#[cfg(test)]
mod tests {
    use super::*;
    use ed25519_dalek::{Signature, VerifyingKey};
    use std::collections::BTreeMap;
    use std::io::Write as _;
    use std::net::{TcpListener, TcpStream};
    use std::sync::{Arc, Mutex};

    fn read_request(socket: &mut TcpStream) -> (String, BTreeMap<String, String>, Vec<u8>) {
        socket
            .set_read_timeout(Some(Duration::from_secs(5)))
            .expect("timeout");
        read_http_request(socket)
    }

    fn read_http_request(
        socket: &mut impl std::io::Read,
    ) -> (String, BTreeMap<String, String>, Vec<u8>) {
        let mut bytes = Vec::new();
        let mut byte = [0_u8; 1];
        while !bytes.ends_with(b"\r\n\r\n") {
            socket.read_exact(&mut byte).expect("request header");
            bytes.push(byte[0]);
        }
        let text = String::from_utf8(bytes).expect("HTTP header");
        let mut lines = text.split("\r\n");
        let path = lines
            .next()
            .expect("request line")
            .split_whitespace()
            .nth(1)
            .expect("path")
            .to_owned();
        let headers: BTreeMap<_, _> = lines
            .filter_map(|line| line.split_once(':'))
            .map(|(name, value)| (name.to_ascii_lowercase(), value.trim().to_owned()))
            .collect();
        let size = headers
            .get("content-length")
            .map_or(0, |value| value.parse::<usize>().expect("length"));
        let mut body = vec![0_u8; size];
        socket.read_exact(&mut body).expect("request body");
        (path, headers, body)
    }

    fn respond(socket: &mut impl std::io::Write, body: &serde_json::Value) {
        let body = serde_json::to_vec(body).expect("response JSON");
        write!(socket, "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n", body.len()).expect("response headers");
        socket.write_all(&body).expect("response body");
    }

    #[cfg(unix)]
    #[test]
    #[allow(clippy::too_many_lines)]
    fn https_supervisor_relays_worker_stdio_with_device_ca_and_signature() {
        use std::os::unix::fs::PermissionsExt as _;
        use winwincode_client_port::worker_relay::{DeviceWorkerRelayRequest, WorkerRelayRequest};

        struct Shutdown(crate::SessionSupervisor);
        impl Drop for Shutdown {
            fn drop(&mut self) {
                let _ = self.0.shutdown();
            }
        }

        let root = std::env::temp_dir().join(format!(
            "wwc-https-worker-relay-{}-{}",
            std::process::id(),
            time::OffsetDateTime::now_utc().unix_timestamp_nanos()
        ));
        std::fs::create_dir_all(&root).expect("fixture directory");
        let binary = root.join("winwincode-worker");
        std::fs::write(&binary, "#!/bin/sh\ntrap 'exit 0' TERM\nprintf '%s\\n' '{\"kind\":\"exchange\",\"body\":\"{}\"}'\nIFS= read -r response\nprintf '%s' \"$response\" > \"$(dirname \"$2\")/relay-response.json\"\nwhile :; do sleep 0.1; done\n").expect("Worker fixture");
        std::fs::set_permissions(&binary, std::fs::Permissions::from_mode(0o755))
            .expect("Worker executable");
        let listener = TcpListener::bind("127.0.0.1:0").expect("HTTPS listener");
        let origin = format!(
            "https://localhost:{}",
            listener.local_addr().expect("listener address").port()
        );
        let node = "cnd_00000000000000000000000000";
        let mut store = DeviceStore::open(&root).expect("Device store");
        store
            .advance_occupancy_mirror(&crate::OccupancyMirrorUpdate {
                occupancy_lease_id: "ocl_AAAAAAAAAAAAAAAAAAAAAAAAAA".to_owned(),
                fencing_token: 7,
                holder_user_id: None,
                claim_request_id: None,
                idle_expires_at: None,
                acknowledged_at: "2026-10-15T00:00:00.000Z".to_owned(),
            })
            .expect("current occupancy");
        let supervisor = crate::SessionSupervisor::new(
            crate::SupervisorConfig {
                client_node_id: node.to_owned(),
                client_instance_id: "cix_00000000000000000000000000".to_owned(),
                server_origin: origin.clone(),
                worker_binary_path: Some(binary),
                stop_grace_period: Duration::from_secs(3),
                ..crate::SupervisorConfig::default()
            },
            store,
        )
        .expect("bound HTTPS origin must start a supervisor");
        let shutdown = Shutdown(supervisor.clone());
        let rcgen::CertifiedKey { cert, signing_key } =
            rcgen::generate_simple_self_signed(vec!["localhost".to_owned()]).expect("TLS identity");
        let key = rustls::pki_types::PrivatePkcs8KeyDer::from(signing_key.serialize_der());
        let tls = rustls::ServerConfig::builder_with_provider(Arc::new(
            rustls::crypto::ring::default_provider(),
        ))
        .with_safe_default_protocol_versions()
        .expect("TLS versions")
        .with_no_client_auth()
        .with_single_cert(vec![cert.der().clone()], key.into())
        .expect("TLS server");
        let signing_key = SigningKey::from_bytes(&[5; 32]);
        let verifying_key = signing_key.verifying_key();
        let auth = DeviceRemoteAuth {
            server_url: origin.clone(),
            signing_key,
            client_node_id: node.to_owned(),
            ca_roots: vec![cert.der().to_vec()],
        };
        supervisor.set_worker_relay_transport(Arc::new(
            crate::HttpExchangeTransport::new(format!("{origin}/internal/v1/client/exchange"))
                .with_device_auth(auth),
        ));
        let server = std::thread::spawn(move || {
            for expected in [CHALLENGE_PATH, "/internal/v1/client/worker-exchange"] {
                let (socket, _) = listener.accept().expect("TLS connection");
                socket
                    .set_read_timeout(Some(Duration::from_secs(5)))
                    .expect("TLS timeout");
                let connection = rustls::ServerConnection::new(Arc::new(tls.clone()))
                    .expect("TLS connection state");
                let mut stream = rustls::StreamOwned::new(connection, socket);
                let (path, headers, body) = read_http_request(&mut stream);
                assert_eq!(path, expected);
                if expected == CHALLENGE_PATH {
                    respond(
                        &mut stream,
                        &serde_json::json!({"schemaVersion":"winwincode/v1", "challengeId":"challenge-https-relay", "challenge":"relay-fixture", "expiresAt":"2099-01-01T00:00:00.000Z"}),
                    );
                } else {
                    assert_eq!(headers["x-winwincode-client-node-id"], node);
                    assert_eq!(
                        headers["x-winwincode-device-public-key"],
                        BASE64.encode(verifying_key.as_bytes())
                    );
                    let signature = Signature::from_slice(
                        &BASE64
                            .decode(&headers["x-winwincode-device-signature"])
                            .expect("signature encoding"),
                    )
                    .expect("signature size");
                    verifying_key
                        .verify_strict(
                            &winwincode_client_port::device_auth::signing_bytes(
                                "challenge-https-relay",
                                "relay-fixture",
                                "POST",
                                expected,
                                node,
                                &body,
                            ),
                            &signature,
                        )
                        .expect("Device signs the exact relay route and body");
                    let relay: DeviceWorkerRelayRequest =
                        serde_json::from_slice(&body).expect("Device relay body");
                    assert_eq!(relay.client_node_id, node);
                    assert_eq!(relay.worker_id, "wrk_TESTWORKER00000000000001");
                    assert_eq!(relay.worker_instance_id, "wki_TESTINSTANCE000000000001");
                    assert_eq!(
                        relay.request,
                        WorkerRelayRequest::Exchange {
                            body: "{}".to_owned()
                        }
                    );
                    respond(
                        &mut stream,
                        &serde_json::json!({"failure":null,"status":202,"body":"relayed"}),
                    );
                }
            }
        });
        let data = root.join("session");
        supervisor
            .spawn(crate::SpawnRequest {
                worker_session_id: "wss_HTTPS_RELAY",
                worker_id: "wrk_TESTWORKER00000000000001",
                worker_instance_id: "wki_TESTINSTANCE000000000001",
                occupancy_lease_id: "ocl_AAAAAAAAAAAAAAAAAAAAAAAAAA",
                occupancy_fencing_token: 7,
                repository_binding_id: "rbd_HTTPS_RELAY",
                launch_grant_id: "wlg_HTTPS_RELAY",
                product_session_id: None,
                work_run_id: None,
                source_directory: &root,
                data_directory: &data,
                worker_root: &root,
            })
            .expect("managed Worker launch");
        let response_path = data.join("relay-response.json");
        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        while !response_path.exists() && std::time::Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(10));
        }
        let response: serde_json::Value = serde_json::from_slice(
            &std::fs::read(response_path).expect("Worker receives HTTPS relay response"),
        )
        .expect("relay response");
        assert_eq!(response["status"], 202);
        assert_eq!(response["body"], "relayed");
        let config =
            std::fs::read_to_string(data.join("managed-session.json")).expect("Worker config");
        assert!(!config.contains(&origin));
        assert!(!config.contains("ca_roots"));
        assert!(!config.contains("signing_key"));
        supervisor.shutdown().expect("Worker cleanup");
        server.join().expect("HTTPS signature fixture");
        drop(shutdown);
        drop(supervisor);
        std::fs::remove_dir_all(root).expect("fixture cleanup");
    }

    #[tokio::test]
    async fn preview_challenge_temporary_http_failure_keeps_retry_classification() {
        use winwincode_client_port::preview::{PreviewSourceDescriptor, PreviewSourceMode};
        use winwincode_network::RetryFailure as _;
        for status in [429, 503] {
            let listener = TcpListener::bind("127.0.0.1:0").expect("challenge failure fixture");
            let address = listener.local_addr().expect("fixture address");
            let server = std::thread::spawn(move || {
                let (mut request, _) = listener.accept().expect("challenge connection");
                assert_eq!(read_request(&mut request).0, CHALLENGE_PATH);
                write!(request, "HTTP/1.1 {status} Temporary Failure\r\nRetry-After: 2\r\nContent-Length: 0\r\nConnection: close\r\n\r\n").expect("temporary challenge failure");
            });
            let source = crate::AuthorizedPreviewSource::new(
                PreviewSourceDescriptor {
                    source_id: "pvs_fixture".to_owned(),
                    work_run_id: "wrn_fixture".to_owned(),
                    repository_binding_id: "rbd_fixture".to_owned(),
                    mode: PreviewSourceMode::Live,
                    candidate_commit: None,
                },
                "127.0.0.1:4173".parse().expect("private preview source"),
            )
            .expect("authorized source");
            let auth = DeviceRemoteAuth {
                server_url: format!("http://{address}"),
                signing_key: SigningKey::from_bytes(&[5; 32]),
                client_node_id: "cnd_00000000000000000000000000".to_owned(),
                ca_roots: Vec::new(),
            };
            let client = crate::PreviewTunnelClient::new(
                format!("ws://{address}/internal/v1/preview/tunnel"),
                auth.client_node_id.clone(),
                [source],
            )
            .expect("preview client")
            .with_device_auth(auth);
            let error = client
                .run_once()
                .await
                .expect_err("temporary challenge failure");
            assert!(error.retryable());
            assert_eq!(error.network_failure().http_status, Some(status));
            assert_eq!(error.network_failure().retry_after_ms, Some(2000));
            server.join().expect("challenge failure fixture thread");
        }
    }

    #[test]
    fn revoked_signed_http_is_not_retried() {
        let listener = TcpListener::bind("127.0.0.1:0").expect("revocation fixture");
        let origin = format!("http://{}", listener.local_addr().expect("fixture address"));
        let server = std::thread::spawn(move || {
            let (mut challenge, _) = listener.accept().expect("challenge connection");
            assert_eq!(read_request(&mut challenge).0, CHALLENGE_PATH);
            respond(
                &mut challenge,
                &serde_json::json!({ "schemaVersion":"winwincode/v1", "challengeId":"challenge-revoked", "challenge":"Y2hhbGxlbmdl", "expiresAt":"2099-01-01T00:00:00.000Z" }),
            );
            let (mut request, _) = listener.accept().expect("signed request");
            assert_eq!(read_request(&mut request).0, "/internal/v1/client/exchange");
            request
                .write_all(
                    b"HTTP/1.1 403 Forbidden\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
                )
                .expect("revocation response");
        });
        let auth = DeviceRemoteAuth {
            server_url: origin.clone(),
            signing_key: SigningKey::from_bytes(&[5; 32]),
            client_node_id: "cnd_00000000000000000000000000".to_owned(),
            ca_roots: Vec::new(),
        };
        let failure = auth
            .post_json(
                &format!("{origin}/internal/v1/client/exchange"),
                b"{}",
                Duration::from_secs(3),
                &|| true,
            )
            .expect_err("revoked Device is rejected");
        assert_eq!(failure.network_failure().http_status, Some(403));
        server.join().expect("revocation fixture thread");
    }

    #[test]
    #[allow(clippy::too_many_lines)]
    fn response_loss_reuses_persisted_key_intent_request_and_server_after_restart() {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind HTTP fixture");
        let server_url = format!("http://{}", listener.local_addr().expect("address"));
        let bodies = Arc::new(Mutex::new(Vec::new()));
        let received = Arc::clone(&bodies);
        let server = std::thread::spawn(move || {
            let mut challenge = String::new();
            for index in 0..6 {
                let (mut socket, _) = listener.accept().expect("accept");
                let (path, headers, body) = read_request(&mut socket);
                if path == CHALLENGE_PATH {
                    challenge = BASE64.encode(format!("challenge-{index}"));
                    respond(
                        &mut socket,
                        &serde_json::json!({ "schemaVersion": "winwincode/v1", "challengeId": format!("challenge-{index}"), "challenge": challenge, "expiresAt":"2099-01-01T00:00:00.000Z" }),
                    );
                } else {
                    assert_eq!(path, BIND_PATH);
                    let key: [u8; 32] = BASE64
                        .decode(&headers["x-winwincode-device-public-key"])
                        .expect("public key")
                        .try_into()
                        .expect("key size");
                    let signature = Signature::from_slice(
                        &BASE64
                            .decode(&headers["x-winwincode-device-signature"])
                            .expect("signature"),
                    )
                    .expect("signature size");
                    let signed = winwincode_client_port::device_auth::signing_bytes(
                        &headers["x-winwincode-device-challenge-id"],
                        &challenge,
                        "POST",
                        BIND_PATH,
                        "",
                        &body,
                    );
                    VerifyingKey::from_bytes(&key)
                        .expect("public key")
                        .verify_strict(&signed, &signature)
                        .expect("private key possession");
                    let request: serde_json::Value =
                        serde_json::from_slice(&body).expect("binding request");
                    received.lock().expect("requests").push(request.clone());
                    if index == 3 {
                        let rejected = br#"{"code":"DEVICE_AUTH_REQUIRED"}"#;
                        write!(socket, "HTTP/1.1 401 Unauthorized\r\nContent-Length: {}\r\nConnection: close\r\n\r\n", rejected.len()).expect("challenge expiry response");
                        socket.write_all(rejected).expect("challenge expiry body");
                    } else if index == 5 {
                        respond(
                            &mut socket,
                            &serde_json::json!({ "schemaVersion":"winwincode/v1", "requestId":request["requestId"], "clientNodeId":"cnd_00000000000000000000000000", "publicClientId":"123456789", "ownerUserId":"usr_owner", "boundAt":"2026-10-15T00:00:00.000Z" }),
                        );
                    }
                }
            }
        });
        let root = std::env::temp_dir().join(format!(
            "wwc-binding-retry-{}-{}",
            std::process::id(),
            time::OffsetDateTime::now_utc().unix_timestamp_nanos()
        ));
        let config = DeviceBindConfig {
            data_directory: root.clone(),
            server_url: Some(server_url.clone()),
            pairing_token: Some("once-only-pairing-token".to_owned()),
            ca_file: None,
            seed: DeviceIdentitySeed {
                display_name: "Fixture Device".to_owned(),
                platform: "aarch64-apple-darwin".to_owned(),
                architecture: "aarch64".to_owned(),
                client_version: "0.1.0".to_owned(),
            },
        };
        assert!(
            bind_device(&config).is_err(),
            "first committed response is lost"
        );
        let pending = load_remote_binding(&DeviceStore::open(&root).expect("reopen pending store"))
            .expect("binding")
            .expect("pending identity");
        assert_eq!(pending.server_url, server_url);
        assert!(!pending.is_bound());
        let resumed = DeviceBindConfig {
            server_url: None,
            pairing_token: Some("replacement-token-must-not-hide-committed-binding".to_owned()),
            ..config
        };
        let receipt = bind_device(&resumed).expect("recover original binding");
        server.join().expect("HTTP fixture thread");
        let requests = bodies.lock().expect("requests");
        assert_eq!(requests.len(), 3);
        assert_eq!(
            requests[0], requests[1],
            "restart replays original token, key, intent, and request ID"
        );
        assert_eq!(
            requests[1], requests[2],
            "expired challenge refresh preserves the exact binding request"
        );
        assert_eq!(receipt.request_id.0, pending.request_id);
        let store = DeviceStore::open(&root).expect("reopen bound store");
        let stored = load_remote_binding(&store)
            .expect("binding")
            .expect("bound");
        assert!(stored.is_bound());
        assert!(stored.pairing_token.is_none());
        assert_eq!(stored.server_url, server_url);
        assert_eq!(
            stored.auth.signing_key.to_bytes(),
            pending.auth.signing_key.to_bytes()
        );
        let identity = load_device_identity(&store)
            .expect("identity")
            .expect("bound identity");
        assert_eq!(
            identity.identity().client_node_id(),
            receipt.client_node_id.0
        );
        record_remote_failure(&store, "Device 身份认证失败；服务已停止。")
            .expect("persist a previous authentication failure");
        assert_eq!(
            bind_device(&resumed)
                .expect("already bound does not contact Server")
                .request_id
                .0,
            receipt.request_id.0
        );
        assert!(
            load_remote_binding(&store)
                .unwrap()
                .unwrap()
                .failure_reason
                .is_none()
        );
        record_remote_failure(&store, DEVICE_REVOKED_REASON)
            .expect("persist authorization failure");
        let status = crate::service::device_service_status(&root);
        assert_eq!(status.server_url.as_deref(), Some(server_url.as_str()));
        assert_eq!(
            status.authorization_failure.as_deref(),
            Some(DEVICE_REVOKED_REASON)
        );
        assert!(
            bind_device(&resumed).is_err(),
            "permanent authorization failure does not reactivate a binding"
        );
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt as _;
            assert_eq!(
                std::fs::metadata(&root)
                    .expect("directory metadata")
                    .permissions()
                    .mode()
                    & 0o777,
                0o700
            );
            assert_eq!(
                std::fs::metadata(store.database_path())
                    .expect("database metadata")
                    .permissions()
                    .mode()
                    & 0o777,
                0o600
            );
        }
        drop(store);
        std::fs::remove_dir_all(root).expect("cleanup");
    }

    #[cfg(feature = "test-support")]
    #[test]
    #[allow(clippy::too_many_lines)]
    fn receipt_save_crash_recovers_original_binding_with_a_replacement_token() {
        let seed = DeviceIdentitySeed {
            display_name: "Crash fixture Device".to_owned(),
            platform: "aarch64-apple-darwin".to_owned(),
            architecture: "aarch64".to_owned(),
            client_version: "0.2.0".to_owned(),
        };
        if let Some(root) = std::env::var_os("WWC_DEVICE_BIND_TEST_DIRECTORY") {
            bind_device(&DeviceBindConfig {
                data_directory: PathBuf::from(root),
                server_url: Some(std::env::var("WWC_DEVICE_BIND_TEST_SERVER").expect("Server")),
                pairing_token: Some(std::env::var("WWC_DEVICE_BIND_TEST_TOKEN").expect("token")),
                ca_file: None,
                seed,
            })
            .expect("recover binding in child process");
            return;
        }
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind HTTP fixture");
        let server_url = format!("http://{}", listener.local_addr().expect("address"));
        let bodies = Arc::new(Mutex::new(Vec::new()));
        let received = Arc::clone(&bodies);
        let server = std::thread::spawn(move || {
            for index in 0..2 {
                let (mut socket, _) = listener.accept().expect("accept challenge");
                assert_eq!(read_request(&mut socket).0, CHALLENGE_PATH);
                respond(
                    &mut socket,
                    &serde_json::json!({"schemaVersion":"winwincode/v1", "challengeId":format!("challenge-{index}"), "challenge":"fixture", "expiresAt":"2099-01-01T00:00:00.000Z"}),
                );
                let (mut socket, _) = listener.accept().expect("accept bind");
                let (path, _, body) = read_request(&mut socket);
                assert_eq!(path, BIND_PATH);
                let request: serde_json::Value = serde_json::from_slice(&body).expect("request");
                received.lock().expect("requests").push(request.clone());
                respond(
                    &mut socket,
                    &serde_json::json!({"schemaVersion":"winwincode/v1", "requestId":request["requestId"], "clientNodeId":"cnd_00000000000000000000000000", "publicClientId":"123456789", "ownerUserId":"usr_owner", "boundAt":"2026-10-15T00:00:00.000Z"}),
                );
            }
        });
        let root = std::env::temp_dir().join(format!(
            "wwc-binding-receipt-crash-{}-{}",
            std::process::id(),
            time::OffsetDateTime::now_utc().unix_timestamp_nanos()
        ));
        let child = |token: &str, crash: bool| {
            let mut command =
                std::process::Command::new(std::env::current_exe().expect("test binary"));
            command
                .args([
                    "--exact",
                    "remote_auth::tests::receipt_save_crash_recovers_original_binding_with_a_replacement_token",
                    "--nocapture",
                ])
                .env("WWC_DEVICE_BIND_TEST_DIRECTORY", &root)
                .env("WWC_DEVICE_BIND_TEST_SERVER", &server_url)
                .env("WWC_DEVICE_BIND_TEST_TOKEN", token);
            if crash {
                command.env("WWC_DEVICE_TEST_EXIT_BEFORE_BIND_RECEIPT_SAVE", "1");
            } else {
                command.env_remove("WWC_DEVICE_TEST_EXIT_BEFORE_BIND_RECEIPT_SAVE");
            }
            command.output().expect("child process")
        };
        assert_eq!(
            child("original-pairing-token", true).status.code(),
            Some(86)
        );
        let store = DeviceStore::open(&root).expect("store");
        let pending = load_remote_binding(&store)
            .expect("binding")
            .expect("pending");
        assert!(!pending.is_bound());
        let recovered = child("replacement-pairing-token", false);
        assert!(
            recovered.status.success(),
            "{}",
            String::from_utf8_lossy(&recovered.stdout)
        );
        server.join().expect("HTTP fixture");
        let requests = bodies.lock().expect("requests");
        assert_eq!(requests.len(), 2);
        assert_eq!(requests[0], requests[1]);
        let stored = load_remote_binding(&store)
            .expect("binding")
            .expect("bound");
        assert!(stored.is_bound());
        assert_eq!(stored.request_id, pending.request_id);
        assert_eq!(
            stored.auth.signing_key.to_bytes(),
            pending.auth.signing_key.to_bytes()
        );
        assert_eq!(
            stored.receipt.expect("receipt").client_node_id.0,
            "cnd_00000000000000000000000000"
        );
        drop(store);
        std::fs::remove_dir_all(root).expect("cleanup");
    }

    #[test]
    #[allow(clippy::too_many_lines)]
    fn unconfirmed_binding_keeps_original_token_after_authentication_failure_or_revocation() {
        for (status, revoked) in [(401, false), (403, false), (403, true)] {
            let listener = TcpListener::bind("127.0.0.1:0").expect("bind HTTP fixture");
            let server_url = format!("http://{}", listener.local_addr().expect("address"));
            let requests = Arc::new(Mutex::new(Vec::new()));
            let received = Arc::clone(&requests);
            let server = std::thread::spawn(move || {
                let attempts = if status == 401 {
                    4
                } else if revoked {
                    1
                } else {
                    2
                };
                for index in 0..attempts {
                    let (mut socket, _) = listener.accept().expect("accept challenge");
                    assert_eq!(read_request(&mut socket).0, CHALLENGE_PATH);
                    respond(
                        &mut socket,
                        &serde_json::json!({"schemaVersion":"winwincode/v1", "challengeId":format!("challenge-{index}"), "challenge":"fixture", "expiresAt":"2099-01-01T00:00:00.000Z"}),
                    );
                    let (mut socket, _) = listener.accept().expect("accept bind");
                    let (path, _, body) = read_request(&mut socket);
                    assert_eq!(path, BIND_PATH);
                    received.lock().expect("requests").push(body);
                    let body = if revoked {
                        br#"{"schemaVersion":"winwincode/v1","code":"DEVICE_REVOKED"}"#.as_slice()
                    } else {
                        b""
                    };
                    write!(socket, "HTTP/1.1 {status} Rejected\r\nContent-Length: {}\r\nConnection: close\r\n\r\n", body.len()).expect("rejection");
                    socket.write_all(body).expect("rejection body");
                }
            });
            let root = std::env::temp_dir().join(format!(
                "wwc-binding-auth-failure-{}-{status}-{revoked}-{}",
                std::process::id(),
                time::OffsetDateTime::now_utc().unix_timestamp_nanos()
            ));
            let config = DeviceBindConfig {
                data_directory: root.clone(),
                server_url: Some(server_url),
                pairing_token: Some("original-pairing-token".to_owned()),
                ca_file: None,
                seed: DeviceIdentitySeed {
                    display_name: "Fixture Device".to_owned(),
                    platform: "aarch64-apple-darwin".to_owned(),
                    architecture: "aarch64".to_owned(),
                    client_version: "0.2.0".to_owned(),
                },
            };
            let first_error = bind_device(&config).expect_err("authentication is rejected");
            assert_eq!(
                requests.lock().expect("requests").len(),
                if status == 401 { 2 } else { 1 },
                "the bind request must reach HTTP before it fails: {first_error}"
            );
            let store = DeviceStore::open(&root).expect("store");
            let pending = load_remote_binding(&store)
                .expect("binding")
                .expect("pending");
            assert!(!pending.is_bound());
            assert_eq!(pending.failure_reason.is_some(), revoked);
            if revoked {
                assert_eq!(
                    pending.failure_reason.as_deref(),
                    Some(DEVICE_REVOKED_REASON)
                );
            }
            if status == 401 {
                record_remote_failure(&store, "Device 身份认证失败；服务已停止。")
                    .expect("persist a previous authentication failure");
            } else if !revoked {
                record_remote_failure(&store, "Device 绑定已吊销；服务已停止。")
                    .expect("persist a previous ambiguous 403 failure");
            }
            let resumed = DeviceBindConfig {
                pairing_token: Some("replacement-pairing-token".to_owned()),
                ..config
            };
            assert!(bind_device(&resumed).is_err());
            assert_eq!(
                requests.lock().expect("requests").len(),
                if status == 401 {
                    4
                } else if revoked {
                    1
                } else {
                    2
                }
            );
            server.join().expect("HTTP fixture");
            let requests = requests.lock().expect("requests");
            assert!(requests.iter().all(|request| request == &requests[0]));
            let stored = load_remote_binding(&store)
                .expect("binding")
                .expect("pending");
            assert_eq!(stored.failure_reason.is_some(), revoked);
            assert_eq!(stored.request_id, pending.request_id);
            assert_eq!(stored.pairing_token, pending.pairing_token);
            assert_eq!(stored.intent_json, pending.intent_json);
            assert_eq!(
                stored.auth.signing_key.to_bytes(),
                pending.auth.signing_key.to_bytes()
            );
            drop(store);
            std::fs::remove_dir_all(root).expect("cleanup");
        }
    }
}
