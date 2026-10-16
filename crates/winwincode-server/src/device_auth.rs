// SPDX-License-Identifier: Apache-2.0

use base64::{Engine as _, engine::general_purpose::STANDARD_NO_PAD as BASE64};
use ed25519_dalek::{Signature, Signer as _, SigningKey, VerifyingKey};
use http::HeaderMap;
use serde_json::Value;
use sha2::{Digest, Sha256};
use std::path::{Path, PathBuf};
use time::OffsetDateTime;
use winwincode_api::generated::{
    DeviceBindRequest, DeviceBindResponse, DeviceChallengeRequest, DeviceChallengeResponse,
    DevicePairingResponse, DeviceRevokeResponse,
};
use winwincode_client_port::device_auth::signing_bytes;
use winwincode_domain::{ClientNodeId, Instant, SchemaVersion};
use winwincode_storage::{
    ClientNodeRegistration, DeviceAuthStoreError, DeviceBindingWrite, SqliteStorage,
};

pub(crate) struct DeviceAuthApplication {
    data_directory: PathBuf,
}

impl DeviceAuthApplication {
    pub(crate) fn new(directory: &Path) -> Self {
        Self {
            data_directory: directory.to_path_buf(),
        }
    }

    fn storage(&self) -> Result<SqliteStorage, DeviceAuthStoreError> {
        Ok(SqliteStorage::open(&self.data_directory)?)
    }

    pub(crate) fn pairing(&self, owner: &str) -> Result<Value, DeviceAuthStoreError> {
        let token = random(32)?;
        let expiry = timestamp(OffsetDateTime::now_utc() + time::Duration::minutes(10))?;
        self.storage()?
            .publish_device_pairing(&digest(token.as_bytes()), owner, &expiry)?;
        serde_json::to_value(DevicePairingResponse {
            schema_version: SchemaVersion::WinwincodeV1,
            pairing_token: token,
            expires_at: Instant(expiry),
        })
        .map_err(|_| DeviceAuthStoreError::Unavailable)
    }

    pub(crate) fn challenge(&self, request: &Value) -> Result<Value, DeviceAuthStoreError> {
        serde_json::from_value::<DeviceChallengeRequest>(request.clone())
            .map_err(|_| DeviceAuthStoreError::InvalidRequest)?;
        let key = field(request, "publicKey")?;
        verifying_key(key).map_err(|_| DeviceAuthStoreError::InvalidRequest)?;
        let now = OffsetDateTime::now_utc();
        let expiry = timestamp(now + time::Duration::seconds(60))?;
        let mut storage = self.storage()?;
        let (id, challenge) = if let Some(secret) = storage.device_challenge_secret(key)? {
            let id = format!("bound_{expiry}_{}", random(24)?);
            let challenge = bound_challenge(&id, key, &secret);
            (id, challenge)
        } else {
            let id = random(24)?;
            let challenge = random(32)?;
            storage.publish_device_challenge(&id, key, &challenge, &expiry, &timestamp(now)?)?;
            (id, challenge)
        };
        serde_json::to_value(DeviceChallengeResponse {
            schema_version: SchemaVersion::WinwincodeV1,
            challenge_id: id,
            challenge,
            expires_at: Instant(expiry),
        })
        .map_err(|_| DeviceAuthStoreError::Unavailable)
    }

    pub(crate) fn authenticate(
        &self,
        headers: &HeaderMap,
        method: &str,
        path: &str,
        node: &str,
        body: &[u8],
    ) -> Result<String, DeviceAuthStoreError> {
        let header = |name| {
            headers
                .get(name)
                .and_then(|v| v.to_str().ok())
                .ok_or(DeviceAuthStoreError::Authentication)
        };
        let key = header("x-winwincode-device-public-key")?;
        let challenge_id = header("x-winwincode-device-challenge-id")?;
        let signature = header("x-winwincode-device-signature")?;
        if !node.is_empty() && header("x-winwincode-client-node-id")? != node {
            return Err(DeviceAuthStoreError::Authentication);
        }
        let mut storage = self.storage()?;
        let now = OffsetDateTime::now_utc();
        let expiry = challenge_id
            .starts_with("bound_")
            .then(|| bound_challenge_expiry(challenge_id, now))
            .transpose()?;
        let now = timestamp(now)?;
        let challenge = if expiry.is_some() {
            let secret = storage
                .device_challenge_secret(key)?
                .ok_or(DeviceAuthStoreError::Authentication)?;
            bound_challenge(challenge_id, key, &secret)
        } else {
            storage.device_challenge(challenge_id, key, &now)?
        };
        let signature = BASE64
            .decode(signature)
            .ok()
            .and_then(|bytes| Signature::from_slice(&bytes).ok())
            .ok_or(DeviceAuthStoreError::Authentication)?;
        verifying_key(key)?
            .verify_strict(
                &signing_bytes(challenge_id, &challenge, method, path, node, body),
                &signature,
            )
            .map_err(|_| DeviceAuthStoreError::Authentication)?;
        if let Some(expiry) = expiry {
            storage.consume_bound_device_challenge(challenge_id, key, &challenge, &expiry, &now)?;
        } else {
            storage.consume_device_challenge(challenge_id, key, &challenge, &now)?;
        }
        if !node.is_empty() {
            storage.authenticate_device_binding(node, key)?;
        }
        Ok(key.to_owned())
    }

    pub(crate) fn bind(&self, request: &Value, key: &str) -> Result<Value, DeviceAuthStoreError> {
        let model = serde_json::from_value::<DeviceBindRequest>(request.clone())
            .map_err(|_| DeviceAuthStoreError::InvalidRequest)?;
        if field(request, "publicKey")? != key {
            return Err(DeviceAuthStoreError::Authentication);
        }
        let request_id = field(request, "requestId")?;
        if !(8..=200).contains(&request_id.len())
            || !request_id.as_bytes()[0].is_ascii_alphanumeric()
            || !request_id
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b"._:/@-".contains(&b))
        {
            return Err(DeviceAuthStoreError::InvalidRequest);
        }
        let intent = &request["intent"];
        let node_id = prefixed_id("cnd_")?;
        let mut digits = [0u8; 12];
        getrandom::fill(&mut digits).map_err(|_| DeviceAuthStoreError::Unavailable)?;
        let public_id = digits
            .iter()
            .map(|byte| char::from(b'0' + byte % 10))
            .collect::<String>();
        let registration = ClientNodeRegistration::try_new(
            &node_id,
            public_id,
            field(intent, "displayName")?,
            field(intent, "platform")?,
            field(intent, "architecture")?,
            field(intent, "clientVersion")?,
            None,
            0,
        )
        .map_err(|_| DeviceAuthStoreError::InvalidRequest)?;
        let now = timestamp(OffsetDateTime::now_utc())?;
        let token_digest = digest(field(request, "pairingToken")?.as_bytes());
        let intent_digest =
            digest(&serde_json::to_vec(&model.intent).map_err(|_| DeviceAuthStoreError::Conflict)?);
        let response = self.storage()?.bind_device_identity(&DeviceBindingWrite {
            request_id,
            public_key: key,
            token_digest: &token_digest,
            intent_digest: &intent_digest,
            registration: &registration,
            access_grant_id: &prefixed_id("cag_")?,
            now: &now,
        })?;
        let response: DeviceBindResponse =
            serde_json::from_str(&response).map_err(|_| DeviceAuthStoreError::Unavailable)?;
        serde_json::to_value(response).map_err(|_| DeviceAuthStoreError::Unavailable)
    }

    pub(crate) fn revoke(&self, node: &str, owner: &str) -> Result<Value, DeviceAuthStoreError> {
        self.storage()?.revoke_device_identity(
            node,
            owner,
            &timestamp(OffsetDateTime::now_utc())?,
        )?;
        serde_json::to_value(DeviceRevokeResponse {
            schema_version: SchemaVersion::WinwincodeV1,
            client_node_id: ClientNodeId(node.to_owned()),
            revoked: true,
        })
        .map_err(|_| DeviceAuthStoreError::Unavailable)
    }
}

fn bound_challenge(id: &str, key: &str, secret: &str) -> String {
    let mut digest = Sha256::new();
    digest.update(b"winwincode/bound-device-challenge-key/v1\0");
    digest.update(secret.as_bytes());
    let seed: [u8; 32] = digest.finalize().into();
    let bytes = format!("winwincode/bound-device-challenge/v1\n{key}\n{id}");
    BASE64.encode(
        SigningKey::from_bytes(&seed)
            .sign(bytes.as_bytes())
            .to_bytes(),
    )
}

fn bound_challenge_expiry(id: &str, now: OffsetDateTime) -> Result<String, DeviceAuthStoreError> {
    let (expiry, nonce) = id
        .strip_prefix("bound_")
        .and_then(|value| value.split_once('_'))
        .ok_or(DeviceAuthStoreError::Authentication)?;
    let expires = OffsetDateTime::parse(expiry, &time::format_description::well_known::Rfc3339)
        .map_err(|_| DeviceAuthStoreError::Authentication)?;
    if timestamp(expires)? != expiry
        || expires <= now
        || expires > now + time::Duration::seconds(60)
        || BASE64
            .decode(nonce)
            .ok()
            .is_none_or(|value| value.len() != 24)
    {
        return Err(DeviceAuthStoreError::Authentication);
    }
    Ok(expiry.to_owned())
}

fn field<'a>(value: &'a Value, name: &str) -> Result<&'a str, DeviceAuthStoreError> {
    value[name]
        .as_str()
        .filter(|v| !v.is_empty())
        .ok_or(DeviceAuthStoreError::InvalidRequest)
}
fn verifying_key(value: &str) -> Result<VerifyingKey, DeviceAuthStoreError> {
    let bytes: [u8; 32] = BASE64
        .decode(value)
        .ok()
        .and_then(|bytes| bytes.try_into().ok())
        .ok_or(DeviceAuthStoreError::Authentication)?;
    VerifyingKey::from_bytes(&bytes).map_err(|_| DeviceAuthStoreError::Authentication)
}
fn random(count: usize) -> Result<String, DeviceAuthStoreError> {
    let mut bytes = vec![0; count];
    getrandom::fill(&mut bytes).map_err(|_| DeviceAuthStoreError::Unavailable)?;
    Ok(BASE64.encode(bytes))
}
fn prefixed_id(prefix: &str) -> Result<String, DeviceAuthStoreError> {
    let mut bytes = [0; 26];
    getrandom::fill(&mut bytes).map_err(|_| DeviceAuthStoreError::Unavailable)?;
    let alphabet = b"0123456789ABCDEFGHJKMNPQRSTVWXYZ";
    Ok(format!(
        "{prefix}{}",
        bytes
            .iter()
            .map(|b| char::from(alphabet[usize::from(b % 32)]))
            .collect::<String>()
    ))
}
fn digest(bytes: &[u8]) -> String {
    format!("{:x}", Sha256::digest(bytes))
}
fn timestamp(value: OffsetDateTime) -> Result<String, DeviceAuthStoreError> {
    let format = time::format_description::parse(
        "[year]-[month]-[day]T[hour]:[minute]:[second].[subsecond digits:3]Z",
    )
    .map_err(|_| DeviceAuthStoreError::Unavailable)?;
    value
        .format(&format)
        .map_err(|_| DeviceAuthStoreError::Unavailable)
}

#[cfg(test)]
mod tests {
    use super::*;
    use ed25519_dalek::SigningKey;
    use serde_json::json;
    use std::sync::atomic::{AtomicU64, Ordering};
    static NEXT: AtomicU64 = AtomicU64::new(1);
    struct Fixture {
        directory: PathBuf,
        app: DeviceAuthApplication,
        key: SigningKey,
    }
    impl Fixture {
        fn new() -> Self {
            let directory = std::env::temp_dir().join(format!(
                "wwc-device-auth-{}-{}",
                std::process::id(),
                NEXT.fetch_add(1, Ordering::Relaxed)
            ));
            let app = DeviceAuthApplication::new(&directory);
            Self {
                directory,
                app,
                key: SigningKey::from_bytes(&[7; 32]),
            }
        }
        fn headers(&self, method: &str, path: &str, node: &str, body: &[u8]) -> HeaderMap {
            let key = BASE64.encode(self.key.verifying_key().as_bytes());
            let challenge = self
                .app
                .challenge(&json!({"schemaVersion":"winwincode/v1","publicKey":key}))
                .unwrap();
            let signature = self.key.sign(&signing_bytes(
                challenge["challengeId"].as_str().unwrap(),
                challenge["challenge"].as_str().unwrap(),
                method,
                path,
                node,
                body,
            ));
            let mut headers = HeaderMap::new();
            for (name, value) in [
                ("x-winwincode-device-public-key", key),
                (
                    "x-winwincode-device-challenge-id",
                    challenge["challengeId"].as_str().unwrap().to_owned(),
                ),
                (
                    "x-winwincode-device-signature",
                    BASE64.encode(signature.to_bytes()),
                ),
                ("x-winwincode-client-node-id", node.to_owned()),
            ] {
                headers.insert(http::HeaderName::from_static(name), value.parse().unwrap());
            }
            headers
        }
        fn binding_request(&self, token: &str) -> Value {
            json!({"schemaVersion":"winwincode/v1","requestId":"req_00000000000000000000000001","pairingToken":token,"publicKey":BASE64.encode(self.key.verifying_key().as_bytes()),"intent":{"displayName":"Device","platform":"aarch64-apple-darwin","architecture":"aarch64","clientVersion":"0.2.0"}})
        }
        fn bind(&self, request: &Value) -> Result<Value, DeviceAuthStoreError> {
            let body = serde_json::to_vec(request).unwrap();
            let headers = self.headers("POST", "/internal/v1/device/bind", "", &body);
            let key =
                self.app
                    .authenticate(&headers, "POST", "/internal/v1/device/bind", "", &body)?;
            self.app.bind(request, &key)
        }
    }
    impl Drop for Fixture {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.directory);
        }
    }

    #[test]
    fn signed_challenge_is_single_use_and_binds_every_request_component() {
        let f = Fixture::new();
        let body = b"exact bytes";
        for (method, path, node, changed_body) in [
            ("GET", "/internal/v1/device/bind", "", body.as_slice()),
            ("POST", "/internal/v1/client/exchange", "", body.as_slice()),
            (
                "POST",
                "/internal/v1/device/bind",
                "cnd_00000000000000000000000001",
                body.as_slice(),
            ),
            ("POST", "/internal/v1/device/bind", "", b"other".as_slice()),
        ] {
            let headers = f.headers("POST", "/internal/v1/device/bind", "", body);
            assert_eq!(
                f.app
                    .authenticate(&headers, method, path, node, changed_body),
                Err(DeviceAuthStoreError::Authentication)
            );
        }
        let headers = f.headers("POST", "/internal/v1/device/bind", "", body);
        assert!(
            f.app
                .authenticate(&headers, "POST", "/internal/v1/device/bind", "", body)
                .is_ok()
        );
        assert_eq!(
            f.app
                .authenticate(&headers, "POST", "/internal/v1/device/bind", "", body),
            Err(DeviceAuthStoreError::Authentication)
        );
    }

    #[test]
    fn anonymous_challenge_capacity_preserves_bound_device_authentication() {
        for multiple_keys in [false, true] {
            let f = Fixture::new();
            let pairing = f.app.pairing("usr_00000000000000000000000001").unwrap();
            let binding = f
                .bind(&f.binding_request(pairing["pairingToken"].as_str().unwrap()))
                .unwrap();
            let node = binding["clientNodeId"].as_str().unwrap();
            let storage = f.app.storage().unwrap();
            let mut database = rusqlite::Connection::open(storage.database_path()).unwrap();
            let tx = database.transaction().unwrap();
            let expiry =
                timestamp(OffsetDateTime::now_utc() + time::Duration::seconds(60)).unwrap();
            {
                let mut insert = tx
                    .prepare("INSERT INTO device_auth_challenges VALUES(?1,?2,'anonymous',?3)")
                    .unwrap();
                for index in 0..4096 {
                    let mut seed = [8; 32];
                    if multiple_keys {
                        seed[..4].copy_from_slice(&u32::try_from(index).unwrap().to_le_bytes());
                    }
                    let key =
                        BASE64.encode(SigningKey::from_bytes(&seed).verifying_key().as_bytes());
                    insert
                        .execute(rusqlite::params![format!("pending-{index}"), key, expiry])
                        .unwrap();
                }
            }
            tx.commit().unwrap();
            let anonymous_key =
                BASE64.encode(SigningKey::from_bytes(&[8; 32]).verifying_key().as_bytes());
            assert_eq!(
                f.app
                    .challenge(&json!({"schemaVersion":"winwincode/v1","publicKey":anonymous_key})),
                Err(DeviceAuthStoreError::Unavailable)
            );
            let headers = f.headers("POST", "/internal/v1/client/exchange", node, b"request");
            assert!(
                f.app
                    .authenticate(
                        &headers,
                        "POST",
                        "/internal/v1/client/exchange",
                        node,
                        b"request"
                    )
                    .is_ok()
            );
            assert_eq!(
                f.app.authenticate(
                    &headers,
                    "POST",
                    "/internal/v1/client/exchange",
                    node,
                    b"request"
                ),
                Err(DeviceAuthStoreError::Authentication)
            );
        }
    }

    #[test]
    fn public_key_impostor_challenges_preserve_concurrent_bound_requests() {
        let f = Fixture::new();
        let pairing = f.app.pairing("usr_00000000000000000000000001").unwrap();
        let binding = f
            .bind(&f.binding_request(pairing["pairingToken"].as_str().unwrap()))
            .unwrap();
        let node = binding["clientNodeId"].as_str().unwrap();
        let requests: Vec<_> = (0..8)
            .map(|_| f.headers("POST", "/internal/v1/client/exchange", node, b"request"))
            .collect();
        let key = BASE64.encode(f.key.verifying_key().as_bytes());
        for _ in 0..128 {
            f.app
                .challenge(&json!({"schemaVersion":"winwincode/v1","publicKey":key}))
                .unwrap();
        }
        let storage = f.app.storage().unwrap();
        let database = rusqlite::Connection::open(storage.database_path()).unwrap();
        let pending: i64 = database
            .query_row(
                "SELECT count(*) FROM device_auth_challenges WHERE public_key=?1",
                [&key],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(pending, 0);
        std::thread::scope(|scope| {
            for headers in &requests {
                let app = &f.app;
                scope.spawn(move || {
                    assert!(
                        app.authenticate(
                            headers,
                            "POST",
                            "/internal/v1/client/exchange",
                            node,
                            b"request"
                        )
                        .is_ok()
                    );
                    assert_eq!(
                        app.authenticate(
                            headers,
                            "POST",
                            "/internal/v1/client/exchange",
                            node,
                            b"request"
                        ),
                        Err(DeviceAuthStoreError::Authentication)
                    );
                });
            }
        });
        let headers = f.headers("POST", "/internal/v1/client/exchange", node, b"request");
        let barrier = std::sync::Barrier::new(8);
        let succeeded = AtomicU64::new(0);
        std::thread::scope(|scope| {
            for _ in 0..8 {
                let app = &f.app;
                let headers = &headers;
                let barrier = &barrier;
                let succeeded = &succeeded;
                scope.spawn(move || {
                    barrier.wait();
                    match app.authenticate(
                        headers,
                        "POST",
                        "/internal/v1/client/exchange",
                        node,
                        b"request",
                    ) {
                        Ok(_) => {
                            succeeded.fetch_add(1, Ordering::Relaxed);
                        }
                        Err(error) => assert_eq!(error, DeviceAuthStoreError::Authentication),
                    }
                });
            }
        });
        assert_eq!(succeeded.load(Ordering::Relaxed), 1);
    }

    #[test]
    fn bound_authentication_continues_after_many_recent_consumptions() {
        let f = Fixture::new();
        let pairing = f.app.pairing("usr_00000000000000000000000001").unwrap();
        let binding = f
            .bind(&f.binding_request(pairing["pairingToken"].as_str().unwrap()))
            .unwrap();
        let node = binding["clientNodeId"].as_str().unwrap();
        let key = BASE64.encode(f.key.verifying_key().as_bytes());
        let storage = f.app.storage().unwrap();
        let mut database = rusqlite::Connection::open(storage.database_path()).unwrap();
        let tx = database.transaction().unwrap();
        let expiry = timestamp(OffsetDateTime::now_utc() + time::Duration::seconds(60)).unwrap();
        {
            let mut insert = tx
                .prepare("INSERT INTO device_auth_challenges VALUES(?1,?2,'signed',?3)")
                .unwrap();
            for index in 0u32..4096 {
                let mut nonce = [0; 24];
                nonce[..4].copy_from_slice(&index.to_le_bytes());
                let id = format!("bound_{expiry}_{}", BASE64.encode(nonce));
                insert.execute(rusqlite::params![id, key, expiry]).unwrap();
            }
        }
        tx.commit().unwrap();
        for _ in 0..16 {
            let headers = f.headers("POST", "/internal/v1/client/exchange", node, b"request");
            assert!(
                f.app
                    .authenticate(
                        &headers,
                        "POST",
                        "/internal/v1/client/exchange",
                        node,
                        b"request"
                    )
                    .is_ok()
            );
            assert_eq!(
                f.app.authenticate(
                    &headers,
                    "POST",
                    "/internal/v1/client/exchange",
                    node,
                    b"request"
                ),
                Err(DeviceAuthStoreError::Authentication)
            );
        }
        let consumed: i64 = database
            .query_row(
                "SELECT count(*) FROM device_auth_challenges WHERE public_key=?1",
                [&key],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(consumed, 4112);
    }

    #[test]
    fn bound_challenges_reject_expired_future_and_malformed_ids() {
        let now = OffsetDateTime::parse(
            "2026-01-01T00:00:00.000Z",
            &time::format_description::well_known::Rfc3339,
        )
        .unwrap();
        let nonce = random(24).unwrap();
        for expiry in [
            now - time::Duration::seconds(1),
            now + time::Duration::seconds(61),
        ] {
            let id = format!("bound_{}_{nonce}", timestamp(expiry).unwrap());
            assert_eq!(
                bound_challenge_expiry(&id, now),
                Err(DeviceAuthStoreError::Authentication)
            );
        }
        let expiry = timestamp(now + time::Duration::seconds(60)).unwrap();
        for id in [
            "bound_invalid".to_owned(),
            format!("bound_{expiry}_invalid"),
            format!("bound_{}_{nonce}", expiry.replace(".000Z", "Z")),
        ] {
            assert_eq!(
                bound_challenge_expiry(&id, now),
                Err(DeviceAuthStoreError::Authentication)
            );
        }
        assert_eq!(
            bound_challenge_expiry(&format!("bound_{expiry}_{nonce}"), now).unwrap(),
            expiry
        );
    }

    #[test]
    fn anonymous_challenges_reclaim_expired_entries() {
        let f = Fixture::new();
        let key = BASE64.encode(f.key.verifying_key().as_bytes());
        let request = json!({"schemaVersion":"winwincode/v1","publicKey":key});
        let first = f.app.challenge(&request).unwrap();
        for _ in 1..128 {
            f.app.challenge(&request).unwrap();
        }
        let storage = f.app.storage().unwrap();
        let database = rusqlite::Connection::open(storage.database_path()).unwrap();
        database
            .execute(
                "UPDATE device_auth_challenges SET expires_at='2000-01-01T00:00:00.000Z'",
                [],
            )
            .unwrap();
        let mut storage = f.app.storage().unwrap();
        assert_eq!(
            storage.device_challenge(
                first["challengeId"].as_str().unwrap(),
                &key,
                &timestamp(OffsetDateTime::now_utc()).unwrap()
            ),
            Err(DeviceAuthStoreError::Authentication)
        );
        assert!(f.app.challenge(&request).is_ok());
        let pending: i64 = database
            .query_row("SELECT count(*) FROM device_auth_challenges", [], |row| {
                row.get(0)
            })
            .unwrap();
        assert_eq!(pending, 1);
    }

    #[test]
    fn lost_binding_response_recovers_original_identity_and_revocation_is_permanent() {
        let mut f = Fixture::new();
        let pairing = f.app.pairing("usr_00000000000000000000000001").unwrap();
        let request = f.binding_request(pairing["pairingToken"].as_str().unwrap());
        let first = f.bind(&request).unwrap();
        assert_eq!(f.bind(&request).unwrap(), first);
        let mut reordered = request.clone();
        reordered["intent"]=serde_json::from_str(r#"{"clientVersion":"0.2.0","platform":"aarch64-apple-darwin","displayName":"Device","architecture":"aarch64"}"#).unwrap();
        assert_eq!(f.bind(&reordered).unwrap(), first);
        let mut changed = request.clone();
        changed["intent"]["displayName"] = json!("other");
        assert_eq!(f.bind(&changed), Err(DeviceAuthStoreError::Conflict));
        let mut changed = request.clone();
        changed["requestId"] = json!("req_00000000000000000000000002");
        assert_eq!(f.bind(&changed), Err(DeviceAuthStoreError::PairingRejected));
        let node = first["clientNodeId"].as_str().unwrap();
        let original = f.key.clone();
        f.key = SigningKey::from_bytes(&[8; 32]);
        let mut changed = request.clone();
        changed["publicKey"] = json!(BASE64.encode(f.key.verifying_key().as_bytes()));
        assert_eq!(f.bind(&changed), Err(DeviceAuthStoreError::Conflict));
        let impostor = f.headers("POST", "/internal/v1/client/exchange", node, b"request");
        assert_eq!(
            f.app.authenticate(
                &impostor,
                "POST",
                "/internal/v1/client/exchange",
                node,
                b"request"
            ),
            Err(DeviceAuthStoreError::Authentication)
        );
        f.key = original;
        let headers = f.headers("POST", "/internal/v1/client/exchange", node, b"request");
        assert!(
            f.app
                .authenticate(
                    &headers,
                    "POST",
                    "/internal/v1/client/exchange",
                    node,
                    b"request"
                )
                .is_ok()
        );
        f.app
            .revoke(node, "usr_00000000000000000000000001")
            .unwrap();
        assert_eq!(f.bind(&request), Err(DeviceAuthStoreError::Revoked));
        let headers = f.headers("POST", "/internal/v1/client/exchange", node, b"request");
        assert_eq!(
            f.app.authenticate(
                &headers,
                "POST",
                "/internal/v1/client/exchange",
                node,
                b"request"
            ),
            Err(DeviceAuthStoreError::Revoked)
        );
    }
}
