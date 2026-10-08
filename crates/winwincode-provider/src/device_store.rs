// SPDX-License-Identifier: Apache-2.0

//! Device-private Provider settings. The server relays authenticated ciphertext only.

use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt, PermissionsExt};
use std::{collections::BTreeMap, fmt, fs, path::Path, time::Duration};

use aes_gcm::{
    Aes256Gcm, Nonce,
    aead::{Aead, KeyInit, Payload},
};
use base64::{Engine as _, engine::general_purpose::STANDARD};
use hkdf::Hkdf;
use p256::{PublicKey, SecretKey, ecdh::diffie_hellman, elliptic_curve::sec1::ToEncodedPoint};
use rusqlite::{Connection, OptionalExtension, params};
use serde::Deserialize;
use sha2::{Digest, Sha256};
use winwincode_api::generated::{
    DeviceConfigurationEnvelope, DeviceProviderConfig, DeviceProviderOutcome,
    DeviceProviderProjection, DeviceProviderProtocol, DeviceProviderReceipt,
    DeviceProviderSnapshot,
};

use crate::{
    HttpsSseProviderAdapter, HttpsSseProviderConfig, HttpsSseProviderLimits,
    HttpsSseProviderTimeouts, ProviderTokenPricing, ResolvedSecret,
};

const CONTEXT: &str = "winwincode.device-provider.v1";
const DEVICE_PROVIDER_TLS_ROOT_DER_ENVIRONMENT: &str = "WWC_DEVICE_PROVIDER_TLS_ROOT_DER_FILE";
const DEVICE_PROVIDER_HTTPS_PROXY_ENVIRONMENT: &str = "WWC_DEVICE_PROVIDER_HTTPS_PROXY";

#[cfg(test)]
#[path = "device_provider_probe_concurrency_tests.rs"]
mod concurrency_tests;

#[cfg(test)]
#[path = "device_provider_refresh_tests.rs"]
mod refresh_tests;

/// Bounded failure: database and crypto errors never expose configuration or keys.
#[derive(Debug, Clone, Copy)]
pub struct DeviceProviderError;

impl fmt::Display for DeviceProviderError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("device Provider operation failed")
    }
}
impl std::error::Error for DeviceProviderError {}

impl From<rusqlite::Error> for DeviceProviderError {
    fn from(_: rusqlite::Error) -> Self {
        Self
    }
}
impl From<std::io::Error> for DeviceProviderError {
    fn from(_: std::io::Error) -> Self {
        Self
    }
}
impl From<serde_json::Error> for DeviceProviderError {
    fn from(_: serde_json::Error) -> Self {
        Self
    }
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct Mutation {
    operation: String,
    config: DeviceProviderConfig,
    api_key: Option<String>,
    custom_headers: Option<BTreeMap<String, String>>,
}

/// One private database, shared by the Device daemon and its managed Workers.
/// Configuration and the decryption key are committed together; only public snapshots leave it.
pub struct DeviceProviderStore {
    pub(crate) connection: Connection,
    key: SecretKey,
}

impl DeviceProviderStore {
    /// Opens or creates the device-private database. Existing unsafe permissions fail closed.
    ///
    /// # Errors
    /// Rejects symbolic links, public files, unknown schemas, and unavailable storage.
    pub fn open(directory: &Path) -> Result<Self, DeviceProviderError> {
        fs::DirBuilder::new()
            .recursive(true)
            .mode(0o700)
            .create(directory)?;
        let meta = fs::symlink_metadata(directory)?;
        if !meta.is_dir() || meta.permissions().mode() & 0o077 != 0 {
            return Err(DeviceProviderError);
        }
        let path = fs::canonicalize(directory)?.join("providers.sqlite3");
        match fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(&path)
        {
            Ok(file) => file.sync_all()?,
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {}
            Err(error) => return Err(error.into()),
        }
        let meta = fs::symlink_metadata(&path)?;
        if !meta.is_file() || meta.permissions().mode() & 0o077 != 0 {
            return Err(DeviceProviderError);
        }
        let mut connection = Connection::open(path)?;
        connection.busy_timeout(Duration::from_secs(5))?;
        connection.execute_batch(
            "PRAGMA journal_mode=WAL; PRAGMA synchronous=FULL; PRAGMA secure_delete=ON;",
        )?;
        let version: i64 = connection.query_row("PRAGMA user_version", [], |row| row.get(0))?;
        let key = if version == 10 {
            load_device_key(&connection)?
        } else {
            // Only an actual migration needs the writer lock. Worker polling,
            // model startup and accounting readers share this database; opening
            // its current schema must not contend with an accounting transaction.
            let transaction =
                connection.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
            migrate_device_store(&transaction)?;
            let key = load_device_key(&transaction)?;
            transaction.commit()?;
            key
        };
        Ok(Self { connection, key })
    }

    /// Returns configuration metadata and the encryption public key, never a credential.
    ///
    /// # Errors
    /// Returns a bounded failure for invalid local state.
    pub fn snapshot(
        &self,
        client_node_id: &str,
    ) -> Result<DeviceProviderSnapshot, DeviceProviderError> {
        let revision = self.revision()?;
        let mut query = self
            .connection
            .prepare("SELECT config, length(secret)>0 FROM providers ORDER BY provider_id")?;
        let rows = query.query_map([], |row| {
            Ok((row.get::<_, String>(0)?, row.get::<_, bool>(1)?))
        })?;
        let mut providers = Vec::new();
        for row in rows {
            let (config, credential_configured) = row?;
            let config = serde_json::from_str(&config)?;
            if !valid_device_provider_config(&config) {
                return Err(DeviceProviderError);
            }
            providers.push(DeviceProviderProjection {
                config,
                credential_configured,
            });
        }
        Ok(DeviceProviderSnapshot {
            client_node_id: client_node_id.to_owned(),
            revision,
            encryption_public_key: STANDARD
                .encode(self.key.public_key().to_encoded_point(false).as_bytes()),
            providers,
        })
    }

    pub(crate) fn revision(&self) -> Result<i64, DeviceProviderError> {
        Ok(self.connection.query_row(
            "SELECT revision FROM identity WHERE singleton=1",
            [],
            |row| row.get(0),
        )?)
    }

    /// Decrypts and applies one command. Exact replays return the durable receipt.
    ///
    /// # Errors
    /// Rejects changed replay identities and storage failures; ordinary user errors are receipts.
    pub fn apply(
        &mut self,
        device: &str,
        envelope: &DeviceConfigurationEnvelope,
    ) -> Result<DeviceProviderReceipt, DeviceProviderError> {
        if !valid_token(&envelope.request_id, 200)
            || envelope.request_id.len() < 8
            || envelope.client_node_id != device
        {
            return Err(DeviceProviderError);
        }
        let digest = format!("{:x}", Sha256::digest(serde_json::to_vec(envelope)?));
        // Serialize mutation and receipt publication. No network work occurs inside this transaction.
        self.connection.execute_batch("BEGIN IMMEDIATE")?;
        let result = self.apply_transaction(envelope, &digest);
        match result {
            Ok((mut receipt, replayed)) => {
                self.connection.execute_batch("COMMIT")?;
                if !replayed && receipt.outcome == DeviceProviderOutcome::Interrupted {
                    let mutation = self.decrypt(envelope)?;
                    if mutation.operation == "authorize" {
                        self.authorize_provider(mutation, &mut receipt)?;
                        return Ok(receipt);
                    }
                    receipt.outcome = match self.test_provider(mutation, &envelope.request_id) {
                        Ok(()) => DeviceProviderOutcome::Tested,
                        Err(_) => DeviceProviderOutcome::ProviderUnavailable,
                    };
                    self.connection.execute(
                        "UPDATE receipts SET receipt=?1 WHERE request_id=?2",
                        params![serde_json::to_string(&receipt)?, envelope.request_id],
                    )?;
                }
                Ok(receipt)
            }
            Err(error) => {
                let _ = self.connection.execute_batch("ROLLBACK");
                Err(error)
            }
        }
    }

    fn apply_transaction(
        &self,
        envelope: &DeviceConfigurationEnvelope,
        digest: &str,
    ) -> Result<(DeviceProviderReceipt, bool), DeviceProviderError> {
        let previous: Option<(String, String)> = self
            .connection
            .query_row(
                "SELECT digest, receipt FROM receipts WHERE request_id=?1",
                [&envelope.request_id],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .optional()?;
        if let Some((original, receipt)) = previous {
            if original != digest {
                return Err(DeviceProviderError);
            }
            return Ok((serde_json::from_str(&receipt)?, true));
        }
        let revision = self.revision()?;
        let outcome = if revision == envelope.expected_revision {
            match self.decrypt(envelope) {
                Ok(mutation) => self.mutate(mutation)?,
                Err(_) => DeviceProviderOutcome::InvalidRequest,
            }
        } else {
            DeviceProviderOutcome::RevisionConflict
        };
        let receipt = DeviceProviderReceipt {
            request_id: envelope.request_id.clone(),
            outcome,
            revision: self.revision()?,
        };
        self.connection.execute(
            "INSERT INTO receipts VALUES (?1, ?2, ?3)",
            params![
                envelope.request_id,
                digest,
                serde_json::to_string(&receipt)?
            ],
        )?;
        Ok((receipt, false))
    }

    fn decrypt(
        &self,
        envelope: &DeviceConfigurationEnvelope,
    ) -> Result<Mutation, DeviceProviderError> {
        self.decrypt_configuration(envelope, CONTEXT)
    }

    /// Opens a configuration encrypted for this Device and operation namespace.
    ///
    /// # Errors
    /// Rejects invalid keys, altered payloads, and invalid decoded data.
    pub fn decrypt_configuration<T: serde::de::DeserializeOwned>(
        &self,
        envelope: &DeviceConfigurationEnvelope,
        context: &str,
    ) -> Result<T, DeviceProviderError> {
        if envelope.ciphertext.len() > 65_536
            || envelope.public_key.len() > 128
            || envelope.nonce.len() > 24
        {
            return Err(DeviceProviderError);
        }
        let public = PublicKey::from_sec1_bytes(
            &STANDARD
                .decode(&envelope.public_key)
                .map_err(|_| DeviceProviderError)?,
        )
        .map_err(|_| DeviceProviderError)?;
        let shared = diffie_hellman(self.key.to_nonzero_scalar(), public.as_affine());
        let aad = format!(
            "{context}\n{}\n{}\n{}",
            envelope.client_node_id, envelope.request_id, envelope.expected_revision
        );
        let mut key = [0u8; 32];
        Hkdf::<Sha256>::new(Some(context.as_bytes()), shared.raw_secret_bytes())
            .expand(aad.as_bytes(), &mut key)
            .map_err(|_| DeviceProviderError)?;
        let cipher = Aes256Gcm::new_from_slice(&key).map_err(|_| DeviceProviderError)?;
        key.fill(0);
        let nonce = STANDARD
            .decode(&envelope.nonce)
            .map_err(|_| DeviceProviderError)?;
        if nonce.len() != 12 {
            return Err(DeviceProviderError);
        }
        let mut plaintext = cipher
            .decrypt(
                Nonce::from_slice(&nonce),
                Payload {
                    msg: &STANDARD
                        .decode(&envelope.ciphertext)
                        .map_err(|_| DeviceProviderError)?,
                    aad: aad.as_bytes(),
                },
            )
            .map_err(|_| DeviceProviderError)?;
        let mutation = serde_json::from_slice(&plaintext);
        plaintext.fill(0);
        Ok(mutation?)
    }

    fn mutate(&self, mutation: Mutation) -> Result<DeviceProviderOutcome, DeviceProviderError> {
        if !valid_device_provider_config(&mutation.config)
            || mutation.custom_headers.as_ref().is_some_and(|headers| {
                crate::provider_https_sse::validate_custom_headers(
                    &headers
                        .iter()
                        .map(|(k, v)| (k.clone(), v.clone()))
                        .collect::<Vec<_>>(),
                )
                .is_err()
            })
        {
            return Ok(DeviceProviderOutcome::InvalidRequest);
        }
        if matches!(
            mutation.config.protocol,
            DeviceProviderProtocol::CodexChatgpt | DeviceProviderProtocol::ChatgptPlan
        ) && (mutation.api_key.is_some()
            || mutation
                .custom_headers
                .as_ref()
                .is_some_and(|headers| !headers.is_empty()))
        {
            return Ok(DeviceProviderOutcome::InvalidRequest);
        }
        match mutation.operation.as_str() {
            "save" => self.save_provider(mutation),
            "test" => Ok(DeviceProviderOutcome::Interrupted),
            "authorize" if mutation.config.protocol == DeviceProviderProtocol::ChatgptPlan => {
                Ok(DeviceProviderOutcome::Interrupted)
            }
            "delete" => {
                self.connection.execute(
                    "DELETE FROM provider_headers WHERE provider_id=?1",
                    [&mutation.config.provider_id],
                )?;
                self.connection.execute(
                    "DELETE FROM providers WHERE provider_id=?1",
                    [&mutation.config.provider_id],
                )?;
                self.connection.execute(
                    "UPDATE identity SET revision=revision+1 WHERE singleton=1",
                    [],
                )?;
                Ok(DeviceProviderOutcome::Deleted)
            }
            _ => Ok(DeviceProviderOutcome::InvalidRequest),
        }
    }

    fn save_provider(
        &self,
        mut mutation: Mutation,
    ) -> Result<DeviceProviderOutcome, DeviceProviderError> {
        let secret = if mutation.config.protocol == DeviceProviderProtocol::CodexChatgpt {
            let Ok(binding) = self.codex_binding(&mutation.config.provider_id) else {
                return Ok(DeviceProviderOutcome::InvalidRequest);
            };
            if binding.resolve().is_err() {
                return Ok(DeviceProviderOutcome::ProviderUnavailable);
            }
            ResolvedSecret::from_bytes(serde_json::to_vec(&binding)?)
                .map_err(|_| DeviceProviderError)?
        } else if mutation.config.protocol == DeviceProviderProtocol::ChatgptPlan {
            let Ok(secret) = self.saved_secret_for_config(&mutation.config) else {
                return Ok(DeviceProviderOutcome::InvalidRequest);
            };
            secret
        } else {
            match mutation.api_key.take() {
                Some(value)
                    if !value.is_empty()
                        && value.len() <= 8192
                        && !value.chars().any(char::is_control) =>
                {
                    ResolvedSecret::from_bytes(value.into_bytes())
                        .map_err(|_| DeviceProviderError)?
                }
                None => {
                    let Ok(secret) = self.saved_secret_for_config(&mutation.config) else {
                        return Ok(DeviceProviderOutcome::InvalidRequest);
                    };
                    secret
                }
                _ => return Ok(DeviceProviderOutcome::InvalidRequest),
            }
        };
        let count: i64 = self.connection.query_row(
            "SELECT count(*) FROM providers WHERE provider_id != ?1",
            [&mutation.config.provider_id],
            |row| row.get(0),
        )?;
        if count >= 100 {
            return Ok(DeviceProviderOutcome::InvalidRequest);
        }
        self.connection.execute("INSERT INTO providers VALUES (?1, ?2, ?3) ON CONFLICT(provider_id) DO UPDATE SET config=excluded.config, secret=excluded.secret",
            params![mutation.config.provider_id, serde_json::to_string(&mutation.config)?, secret.expose()])?;
        if matches!(
            mutation.config.protocol,
            DeviceProviderProtocol::CodexChatgpt | DeviceProviderProtocol::ChatgptPlan
        ) {
            self.connection.execute(
                "DELETE FROM provider_headers WHERE provider_id=?1",
                [&mutation.config.provider_id],
            )?;
        }
        if let Some(headers) = &mutation.custom_headers {
            self.connection.execute(
                "INSERT INTO provider_headers VALUES (?1, ?2) ON CONFLICT(provider_id) DO UPDATE SET headers=excluded.headers",
                params![mutation.config.provider_id, serde_json::to_string(headers)?],
            )?;
        }
        self.connection.execute(
            "UPDATE identity SET revision=revision+1 WHERE singleton=1",
            [],
        )?;
        Ok(DeviceProviderOutcome::Saved)
    }

    fn saved_secret_for_config(
        &self,
        config: &DeviceProviderConfig,
    ) -> Result<ResolvedSecret, DeviceProviderError> {
        // Configuration saves hold SQLite's writer transaction. Read only local
        // state here, preserving the credential bytes used by rotation's CAS.
        let (stored_config, bytes): (String, Vec<u8>) = self.connection.query_row(
            "SELECT config, secret FROM providers WHERE provider_id=?1",
            [&config.provider_id],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )?;
        let secret = ResolvedSecret::from_bytes(bytes).map_err(|_| DeviceProviderError)?;
        let stored_config: DeviceProviderConfig = serde_json::from_str(&stored_config)?;
        if !valid_device_provider_config(&stored_config)
            || stored_config.protocol != config.protocol
        {
            return Err(DeviceProviderError);
        }
        if config.protocol == DeviceProviderProtocol::ChatgptPlan {
            // Validate the protected record without re-encoding an unchanged grant.
            let _: crate::chatgpt_oauth::Credentials = serde_json::from_slice(secret.expose())?;
        }
        Ok(secret)
    }

    fn test_credentials(
        &self,
        mutation: &mut Mutation,
    ) -> Result<(ResolvedSecret, Option<String>), DeviceProviderError> {
        let (secret, codex_account) =
            if mutation.config.protocol == DeviceProviderProtocol::CodexChatgpt {
                if mutation.api_key.is_some() {
                    return Err(DeviceProviderError);
                }
                let (secret, account) = self
                    .codex_binding(&mutation.config.provider_id)?
                    .resolve()?;
                (secret, Some(account))
            } else if mutation.config.protocol == DeviceProviderProtocol::ChatgptPlan {
                if mutation.api_key.is_some() {
                    return Err(DeviceProviderError);
                }
                let (config, secret, _) = self.resolve_connection(&mutation.config.provider_id)?;
                if config.protocol != DeviceProviderProtocol::ChatgptPlan {
                    return Err(DeviceProviderError);
                }
                (secret, None)
            } else {
                (
                    match mutation.api_key.take() {
                        Some(key)
                            if !key.is_empty()
                                && key.len() <= 8192
                                && !key.chars().any(char::is_control) =>
                        {
                            ResolvedSecret::from_bytes(key.into_bytes())
                                .map_err(|_| DeviceProviderError)?
                        }
                        None => {
                            let (config, secret) = self.resolve(&mutation.config.provider_id)?;
                            if config.protocol != mutation.config.protocol {
                                return Err(DeviceProviderError);
                            }
                            secret
                        }
                        _ => return Err(DeviceProviderError),
                    },
                    None,
                )
            };
        Ok((secret, codex_account))
    }

    fn test_provider(
        &self,
        mut mutation: Mutation,
        request_id: &str,
    ) -> Result<(), DeviceProviderError> {
        use crate::{
            CredentialLeakGate, ProviderAdapterInvocation, ProviderAdapterPort,
            ProviderGatewayOpenReceipt, ProviderStreamControlAction,
        };
        use winwincode_api::generated::ModelRoute;
        use winwincode_domain::{CredentialReferenceId, ModelExchangeId, RequestId};
        if !valid_device_provider_config(&mutation.config) {
            return Err(DeviceProviderError);
        }
        let (secret, codex_account) = self.test_credentials(&mut mutation)?;
        let model = mutation
            .config
            .model_ids
            .first()
            .ok_or(DeviceProviderError)?
            .clone();
        let id = format!(
            "0{}",
            &format!("{:X}", Sha256::digest(request_id.as_bytes()))[..25]
        );
        let exchange = ModelExchangeId(format!("mdl_{id}"));
        let request = RequestId(format!("req_{id}"));
        let adapter_id = format!("device-probe-{id}");
        let payload = serde_json::to_vec(
            &serde_json::json!({"requestId":request.0,"provider":mutation.config.provider_id,"sessionId":"provider-connection-test","threadId":"provider-connection-test", "request":{"model":model,"instructions":"Reply concisely.","input":[{"type":"message","role":"user","content":[{"type":"input_text","text":"Reply OK."}]}],"stream":true,"store":false,"tool_choice":"none","parallel_tool_calls":false}}),
        )?;
        let headers = match mutation.custom_headers {
            Some(headers) => headers,
            None => self.custom_headers(&mutation.config.provider_id)?,
        };
        let _permit = match self.try_provider_model_permit(&mutation.config.provider_id)? {
            crate::DeviceModelAdmission::Ready(permit) => permit,
            crate::DeviceModelAdmission::Deferred => return Err(DeviceProviderError),
        };
        let adapter = adapter(&mutation.config, headers, codex_account)?;
        let mut leak_gate = CredentialLeakGate::new();
        leak_gate.track_secret(&secret);
        adapter
            .open(
                &ProviderAdapterInvocation {
                    model_exchange_id: &exchange,
                    request_id: &request,
                    adapter_request_id: &adapter_id,
                    model_id: &model,
                    content_type: "application/json",
                    payload: &payload,
                },
                &secret,
            )
            .map_err(|_| DeviceProviderError)?;
        drop(secret);
        let receipt = ProviderGatewayOpenReceipt {
            model_exchange_id: exchange.clone(),
            request_id: request,
            route: ModelRoute {
                provider_id: mutation.config.provider_id,
                model_id: model,
                credential_reference_id: CredentialReferenceId(format!("crd_{id}")),
            },
            adapter_request_id: adapter_id.clone(),
            idempotent_replay: false,
            stream_leak_gate: leak_gate,
        };
        let result = adapter.drain_canonical(&receipt);
        let _ = adapter.control(&exchange, &adapter_id, ProviderStreamControlAction::Release);
        let completion = result.map_err(|_| DeviceProviderError)?;
        if completion.terminal.outcome() != crate::ProviderGatewayTerminalOutcome::Succeeded {
            return Err(DeviceProviderError);
        }
        Ok(())
    }

    pub(crate) fn custom_headers(
        &self,
        provider_id: &str,
    ) -> Result<BTreeMap<String, String>, DeviceProviderError> {
        let json: Option<String> = self
            .connection
            .query_row(
                "SELECT headers FROM provider_headers WHERE provider_id=?1",
                [provider_id],
                |row| row.get(0),
            )
            .optional()?;
        Ok(match json {
            Some(value) => serde_json::from_str(&value)?,
            None => BTreeMap::new(),
        })
    }

    /// Reads one configured route for a device-local request.
    ///
    /// # Errors
    /// Rejects a missing or corrupt Provider. The caller must check enabled/model selection.
    pub fn resolve(
        &self,
        provider_id: &str,
    ) -> Result<(DeviceProviderConfig, ResolvedSecret), DeviceProviderError> {
        let (config, secret, _) = self.resolve_connection(provider_id)?;
        Ok((config, secret))
    }

    pub(crate) fn resolve_connection(
        &self,
        provider_id: &str,
    ) -> Result<(DeviceProviderConfig, ResolvedSecret, Option<String>), DeviceProviderError> {
        let (config, secret): (String, Vec<u8>) = self.connection.query_row(
            "SELECT config, secret FROM providers WHERE provider_id=?1",
            [provider_id],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )?;
        let mut secret = secret;
        let config: DeviceProviderConfig = serde_json::from_str(&config)?;
        if !valid_device_provider_config(&config) {
            return Err(DeviceProviderError);
        }
        if config.protocol == DeviceProviderProtocol::CodexChatgpt {
            let binding = serde_json::from_slice::<crate::codex_login::CodexLoginBinding>(&secret);
            secret.fill(0);
            let (secret, account) = binding?.resolve()?;
            Ok((config, secret, Some(account)))
        } else if config.protocol == DeviceProviderProtocol::ChatgptPlan {
            secret.fill(0);
            self.resolve_chatgpt_connection(provider_id, crate::chatgpt_oauth::Credentials::refresh)
        } else {
            let secret = ResolvedSecret::from_bytes(secret).map_err(|_| DeviceProviderError)?;
            Ok((config, secret, None))
        }
    }

    fn resolve_chatgpt_connection(
        &self,
        provider_id: &str,
        refresh: impl FnOnce(&mut crate::chatgpt_oauth::Credentials) -> Result<(), DeviceProviderError>,
    ) -> Result<(DeviceProviderConfig, ResolvedSecret, Option<String>), DeviceProviderError> {
        use crate::device_model_concurrency::{private_directory, private_slot_file};
        use sha2::{Digest as _, Sha256};
        let database = Path::new(self.connection.path().ok_or(DeviceProviderError)?);
        let locks = database
            .parent()
            .ok_or(DeviceProviderError)?
            .join("provider-credential-locks");
        private_directory(&locks)?;
        let owner = private_slot_file(
            &locks.join(format!("{:x}", Sha256::digest(provider_id.as_bytes()))),
        )?;
        owner.lock()?;
        // Reload under the credential lock. Network I/O holds no SQLite transaction.
        let (current_config, mut previous): (String, Vec<u8>) = self.connection.query_row(
            "SELECT config, secret FROM providers WHERE provider_id=?1",
            [provider_id],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )?;
        let result = (|| {
            let mut config: DeviceProviderConfig = serde_json::from_str(&current_config)?;
            if !valid_device_provider_config(&config)
                || config.protocol != DeviceProviderProtocol::ChatgptPlan
            {
                return Err(DeviceProviderError);
            }
            let mut record: crate::chatgpt_oauth::Credentials = serde_json::from_slice(&previous)?;
            if record.needs_refresh()? {
                refresh(&mut record)?;
                // Ordinary saves may change configuration while the grant is in flight.
                // Serialize only this local check/write, preserving the newest config
                // whenever the credential itself and the protocol are still current.
                let transaction = rusqlite::Transaction::new_unchecked(
                    &self.connection,
                    rusqlite::TransactionBehavior::Immediate,
                )?;
                let (latest_config, latest_secret): (String, Vec<u8>) = transaction.query_row(
                    "SELECT config, secret FROM providers WHERE provider_id=?1",
                    [provider_id],
                    |row| Ok((row.get(0)?, row.get(1)?)),
                )?;
                let latest_secret =
                    ResolvedSecret::from_bytes(latest_secret).map_err(|_| DeviceProviderError)?;
                let latest_config: DeviceProviderConfig = serde_json::from_str(&latest_config)?;
                if latest_secret.expose() != previous
                    || !valid_device_provider_config(&latest_config)
                    || latest_config.protocol != DeviceProviderProtocol::ChatgptPlan
                {
                    return Err(DeviceProviderError);
                }
                let mut bytes = serde_json::to_vec(&record)?;
                let saved = transaction.execute(
                    "UPDATE providers SET secret=?1 WHERE provider_id=?2 AND secret=?3",
                    params![bytes, provider_id, previous],
                );
                bytes.fill(0);
                if saved? != 1 {
                    return Err(DeviceProviderError);
                }
                transaction.commit()?;
                config = latest_config;
            }
            Ok((config, record.secret()?, None))
        })();
        previous.fill(0);
        result
    }

    fn codex_binding(
        &self,
        provider_id: &str,
    ) -> Result<crate::codex_login::CodexLoginBinding, DeviceProviderError> {
        let previous: Option<(String, Vec<u8>)> = self
            .connection
            .query_row(
                "SELECT config, secret FROM providers WHERE provider_id=?1",
                [provider_id],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .optional()?;
        if let Some((config, mut secret)) = previous {
            let config: DeviceProviderConfig = serde_json::from_str(&config)?;
            if config.protocol == DeviceProviderProtocol::CodexChatgpt {
                let binding = serde_json::from_slice(&secret);
                secret.fill(0);
                return Ok(binding?);
            }
            secret.fill(0);
        }
        crate::codex_login::CodexLoginBinding::current()
    }

    fn chatgpt_credentials(
        &self,
        provider_id: &str,
    ) -> Result<crate::chatgpt_oauth::Credentials, DeviceProviderError> {
        let (config, mut bytes): (String, Vec<u8>) = self.connection.query_row(
            "SELECT config, secret FROM providers WHERE provider_id=?1",
            [provider_id],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )?;
        let config: DeviceProviderConfig = serde_json::from_str(&config)?;
        let result = if config.protocol == DeviceProviderProtocol::ChatgptPlan {
            serde_json::from_slice(&bytes).map_err(|_| DeviceProviderError)
        } else {
            Err(DeviceProviderError)
        };
        bytes.fill(0);
        result
    }

    fn authorize_provider(
        &self,
        mutation: Mutation,
        receipt: &mut DeviceProviderReceipt,
    ) -> Result<(), DeviceProviderError> {
        let previous = self
            .connection
            .query_row(
                "SELECT config FROM providers WHERE provider_id=?1",
                [&mutation.config.provider_id],
                |row| row.get::<_, String>(0),
            )
            .optional()?;
        let previous = match previous {
            Some(config)
                if serde_json::from_str::<DeviceProviderConfig>(&config)?.protocol
                    == DeviceProviderProtocol::ChatgptPlan =>
            {
                Some(self.chatgpt_credentials(&mutation.config.provider_id)?)
            }
            _ => None,
        };
        let public = self.key.public_key().to_encoded_point(false);
        let thumbprint = serde_json::json!({"crv":"P-256", "kty":"EC", "x":base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(public.x().ok_or(DeviceProviderError)?), "y":base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(public.y().ok_or(DeviceProviderError)?)});
        let host = format!(
            "urn:ietf:params:oauth:jwk-thumbprint:sha-256:{}",
            base64::engine::general_purpose::URL_SAFE_NO_PAD
                .encode(Sha256::digest(serde_json::to_vec(&thumbprint)?))
        );
        let result = crate::chatgpt_oauth::authorize(&host, previous.as_ref()).and_then(|record| {
            crate::chatgpt_oauth::models(&record).map(|models| (record, models))
        });
        self.finish_authorization(mutation, receipt, result)
    }

    fn finish_authorization(
        &self,
        mut mutation: Mutation,
        receipt: &mut DeviceProviderReceipt,
        result: Result<(crate::chatgpt_oauth::Credentials, Vec<String>), DeviceProviderError>,
    ) -> Result<(), DeviceProviderError> {
        self.connection.execute_batch("BEGIN IMMEDIATE")?;
        let saved = (|| {
            if self.revision()? != receipt.revision {
                receipt.outcome = DeviceProviderOutcome::RevisionConflict;
            } else if let Ok((record, models)) = result {
                if models.is_empty() {
                    return Err(DeviceProviderError);
                }
                let count: i64 = self.connection.query_row(
                    "SELECT count(*) FROM providers WHERE provider_id != ?1",
                    [&mutation.config.provider_id],
                    |row| row.get(0),
                )?;
                if count >= 100 {
                    receipt.outcome = DeviceProviderOutcome::InvalidRequest;
                } else {
                    mutation
                        .config
                        .model_ids
                        .retain(|model| models.contains(model));
                    if mutation.config.model_ids.is_empty() {
                        mutation.config.model_ids.push(models[0].clone());
                    }
                    let mut bytes = serde_json::to_vec(&record)?;
                    let save = self.connection.execute("INSERT INTO providers VALUES (?1,?2,?3) ON CONFLICT(provider_id) DO UPDATE SET config=excluded.config, secret=excluded.secret", params![mutation.config.provider_id, serde_json::to_string(&mutation.config)?, bytes]);
                    bytes.fill(0);
                    save?;
                    self.connection.execute(
                        "DELETE FROM provider_headers WHERE provider_id=?1",
                        [&mutation.config.provider_id],
                    )?;
                    self.connection.execute(
                        "UPDATE identity SET revision=revision+1 WHERE singleton=1",
                        [],
                    )?;
                    receipt.outcome = DeviceProviderOutcome::Saved;
                }
            } else {
                receipt.outcome = DeviceProviderOutcome::ProviderUnavailable;
            }
            receipt.revision = self.revision()?;
            self.connection.execute(
                "UPDATE receipts SET receipt=?1 WHERE request_id=?2",
                params![serde_json::to_string(receipt)?, receipt.request_id],
            )?;
            Ok(())
        })();
        match saved {
            Ok(()) => {
                self.connection.execute_batch("COMMIT")?;
                Ok(())
            }
            Err(error) => {
                let _ = self.connection.execute_batch("ROLLBACK");
                Err(error)
            }
        }
    }
}

fn new_secret_key() -> Result<SecretKey, DeviceProviderError> {
    loop {
        let mut bytes = [0u8; 32];
        getrandom::fill(&mut bytes).map_err(|_| DeviceProviderError)?;
        let key = SecretKey::from_slice(&bytes);
        bytes.fill(0);
        if let Ok(key) = key {
            return Ok(key);
        }
    }
}

fn valid_token(value: &str, max: usize) -> bool {
    !value.is_empty()
        && value.len() <= max
        && value.trim() == value
        && !value.chars().any(char::is_control)
}

/// Validates public Provider configuration at Device and projection boundaries.
#[must_use]
pub fn valid_device_provider_config(config: &DeviceProviderConfig) -> bool {
    valid_token(&config.provider_id, 128)
        && valid_token(&config.display_name, 200)
        && !config.model_ids.is_empty()
        && config.model_ids.len() <= 100
        && config.model_ids.iter().all(|model| valid_token(model, 128))
        && config
            .model_ids
            .iter()
            .collect::<std::collections::BTreeSet<_>>()
            .len()
            == config.model_ids.len()
        && config.endpoint.len() <= 2048
        && crate::canonical_https_endpoint(&config.endpoint)
        && (config.protocol != DeviceProviderProtocol::CodexChatgpt
            || config.endpoint == crate::codex_login::CODEX_ENDPOINT)
        && (config.protocol != DeviceProviderProtocol::ChatgptPlan
            || config.endpoint == crate::chatgpt_oauth::ENDPOINT)
}

pub(crate) fn adapter(
    config: &DeviceProviderConfig,
    headers: BTreeMap<String, String>,
    codex_account: Option<String>,
) -> Result<HttpsSseProviderAdapter, DeviceProviderError> {
    // Reasoning tokens count toward this per-response limit. The previous
    // 8,192-token value truncated real Fusion review responses mid-turn.
    const MAX_OUTPUT_TOKENS: u32 = 32_768;
    let mut transport = HttpsSseProviderConfig::try_new(
        config.provider_id.clone(),
        config.endpoint.clone(),
        HttpsSseProviderTimeouts {
            connect: Duration::from_secs(15),
            idle: Duration::from_mins(1),
            total: Duration::from_mins(5),
        },
        HttpsSseProviderLimits {
            response_bytes: 16 * 1024 * 1024,
            event_bytes: 1024 * 1024,
            events: 65536,
        },
    )
    .map_err(|_| DeviceProviderError)?;
    // Task progress has no total development deadline; opening and progress idle timeouts stay finite.
    transport = transport.without_deadlines();
    if let Some(proxy_url) = std::env::var_os(DEVICE_PROVIDER_HTTPS_PROXY_ENVIRONMENT) {
        transport = transport
            .with_http_connect_proxy(&proxy_url.into_string().map_err(|_| DeviceProviderError)?)
            .map_err(|_| DeviceProviderError)?;
    }
    if let Some(path) = std::env::var_os(DEVICE_PROVIDER_TLS_ROOT_DER_ENVIRONMENT) {
        transport = transport
            .with_specific_tls_roots(vec![fs::read(path)?])
            .map_err(|_| DeviceProviderError)?;
    }
    transport = transport
        .with_custom_headers(headers)
        .map_err(|_| DeviceProviderError)?;
    transport = match config.protocol {
        DeviceProviderProtocol::AnthropicMessages => {
            transport.with_anthropic_messages(MAX_OUTPUT_TOKENS, ProviderTokenPricing::default())
        }
        DeviceProviderProtocol::OpenaiChatCompletions => transport
            .with_openai_chat_completions(MAX_OUTPUT_TOKENS, ProviderTokenPricing::default()),
        DeviceProviderProtocol::CodexChatgpt => {
            transport.with_codex_chatgpt(codex_account.ok_or(DeviceProviderError)?)
        }
        DeviceProviderProtocol::ChatgptPlan => transport.with_chatgpt_plan(),
        DeviceProviderProtocol::Canonical => Ok(transport),
    }
    .map_err(|_| DeviceProviderError)?;
    HttpsSseProviderAdapter::try_new(transport).map_err(|_| DeviceProviderError)
}

fn load_device_key(connection: &Connection) -> Result<SecretKey, DeviceProviderError> {
    let mut bytes: Vec<u8> = connection.query_row(
        "SELECT private_key FROM identity WHERE singleton=1",
        [],
        |row| row.get(0),
    )?;
    let key = SecretKey::from_slice(&bytes).map_err(|_| DeviceProviderError);
    bytes.fill(0);
    key
}

fn migrate_device_store(
    transaction: &rusqlite::Transaction<'_>,
) -> Result<(), DeviceProviderError> {
    let version: i64 = transaction.query_row("PRAGMA user_version", [], |row| row.get(0))?;
    if version == 0 {
        transaction.execute_batch(
                "CREATE TABLE identity (singleton INTEGER PRIMARY KEY CHECK(singleton=1), private_key BLOB NOT NULL, revision INTEGER NOT NULL CHECK(revision>=0));
                 CREATE TABLE providers (provider_id TEXT PRIMARY KEY, config TEXT NOT NULL, secret BLOB NOT NULL);
                 CREATE TABLE receipts (request_id TEXT PRIMARY KEY, digest TEXT NOT NULL, receipt TEXT NOT NULL);
                 CREATE TABLE exchanges (exchange_id TEXT PRIMARY KEY, digest TEXT NOT NULL, chunks TEXT, cancelled INTEGER NOT NULL DEFAULT 0);
                 PRAGMA user_version=1;"
            )?;
        let key = new_secret_key()?;
        transaction.execute(
            "INSERT INTO identity VALUES (1, ?1, 0)",
            [key.to_bytes().as_slice()],
        )?;
    } else if ![1, 2, 3, 4, 5, 6, 7, 8, 9, 10].contains(&version) {
        return Err(DeviceProviderError);
    }
    if version < 2 {
        transaction.execute_batch(
                "CREATE TABLE extension_state (singleton INTEGER PRIMARY KEY CHECK(singleton=1), revision INTEGER NOT NULL CHECK(revision>=0));
                 INSERT INTO extension_state VALUES (1, 0);
                 CREATE TABLE extensions (kind TEXT NOT NULL, id TEXT NOT NULL, data TEXT NOT NULL, projection TEXT NOT NULL, PRIMARY KEY(kind,id));
                 CREATE TABLE extension_receipts (request_id TEXT PRIMARY KEY, digest TEXT NOT NULL, receipt TEXT NOT NULL);
                 PRAGMA user_version=2;",
            )?;
    }
    if version < 3 {
        transaction.execute_batch(
            "CREATE TABLE provider_headers (provider_id TEXT PRIMARY KEY, headers TEXT NOT NULL);
                 PRAGMA user_version=3;",
        )?;
    }
    if version < 4 {
        transaction.execute_batch(
                "CREATE TABLE jev_context_exchanges (operation_id TEXT PRIMARY KEY, digest TEXT NOT NULL, result TEXT);
                 PRAGMA user_version=4;",
            )?;
    }
    if version < 5 {
        transaction.execute_batch(
            "CREATE TABLE jev_settings (provider_id TEXT PRIMARY KEY, settings TEXT NOT NULL);
                 PRAGMA user_version=5;",
        )?;
    }
    if version < 6 {
        transaction.execute_batch(
            "ALTER TABLE exchanges ADD COLUMN request_open TEXT;
                 ALTER TABLE exchanges ADD COLUMN prepared_payload BLOB;
                 PRAGMA user_version=6;",
        )?;
    }
    if version < 7 {
        transaction.execute_batch(
            "ALTER TABLE jev_context_exchanges ADD COLUMN request_json TEXT;
                 PRAGMA user_version=7;",
        )?;
    }
    if version < 8 {
        transaction.execute_batch(
                "CREATE TABLE jev_judge_exchanges (operation_id TEXT PRIMARY KEY, request_json TEXT NOT NULL, result TEXT);
                 PRAGMA user_version=8;",
            )?;
    }
    if version < 9 {
        transaction.execute_batch("ALTER TABLE exchanges ADD COLUMN accounting_chunks TEXT;
            CREATE TABLE accounting_closed_attempts (job_id TEXT NOT NULL,attempt INTEGER NOT NULL,lease_id TEXT NOT NULL,PRIMARY KEY(job_id,attempt));
            PRAGMA user_version=9;")?;
    }
    if version < 10 {
        transaction.execute_batch(
            "CREATE TABLE IF NOT EXISTS model_invocation_attempts (
            exchange_id TEXT NOT NULL, attempt_number INTEGER NOT NULL CHECK(attempt_number>0),
            adapter_request_id TEXT NOT NULL UNIQUE, state TEXT NOT NULL,
            failure_chunks TEXT, accounting_chunks TEXT, response_bytes BLOB,
            PRIMARY KEY(exchange_id,attempt_number));
            PRAGMA user_version=10;",
        )?;
    }
    Ok(())
}

#[cfg(test)]
mod codex_tests {
    use super::*;
    fn config() -> DeviceProviderConfig {
        DeviceProviderConfig {
            provider_id: "codex-chatgpt".into(),
            display_name: "Codex ChatGPT".into(),
            endpoint: crate::codex_login::CODEX_ENDPOINT.into(),
            protocol: DeviceProviderProtocol::CodexChatgpt,
            model_ids: vec![
                std::env::var("WWC_CODEX_TEST_MODEL").unwrap_or_else(|_| "gpt-6.1-sol".into()),
            ],
            enabled: true,
        }
    }
    #[test]
    fn codex_endpoint_cannot_be_redirected() {
        let mut config = config();
        assert!(valid_device_provider_config(&config));
        config.endpoint = "https://example.com/responses".into();
        assert!(!valid_device_provider_config(&config));
    }
    #[test]
    fn saved_binding_keeps_tokens_out_of_database_and_refuses_credential_reuse() {
        use base64::{Engine as _, engine::general_purpose::URL_SAFE_NO_PAD};
        let directory =
            std::env::temp_dir().join(format!("wwc-codex-binding-{}", std::process::id()));
        let home = directory.join("codex-home");
        fs::create_dir_all(&home).unwrap();
        fs::set_permissions(&directory, fs::Permissions::from_mode(0o700)).unwrap();
        let claims = serde_json::json!({"exp":u64::MAX,"https://api.openai.com/auth":{"chatgpt_account_id":"account-one"}});
        let token = format!(
            "e30.{}.test",
            URL_SAFE_NO_PAD.encode(serde_json::to_vec(&claims).unwrap())
        );
        let auth = serde_json::json!({"auth_mode":"chatgpt","tokens":{"account_id":"account-one","access_token":token,"refresh_token":"refresh-must-stay-in-codex"}});
        fs::write(home.join("auth.json"), serde_json::to_vec(&auth).unwrap()).unwrap();
        fs::set_permissions(home.join("auth.json"), fs::Permissions::from_mode(0o600)).unwrap();
        let store = DeviceProviderStore::open(&directory).unwrap();
        let binding = serde_json::json!({"codexHome":fs::canonicalize(&home).unwrap(),"accountId":"account-one"});
        store
            .connection
            .execute(
                "INSERT INTO providers VALUES (?1,?2,?3)",
                params![
                    config().provider_id,
                    serde_json::to_string(&config()).unwrap(),
                    serde_json::to_vec(&binding).unwrap()
                ],
            )
            .unwrap();
        assert_eq!(
            store
                .mutate(Mutation {
                    operation: "save".into(),
                    config: config(),
                    api_key: None,
                    custom_headers: None,
                })
                .unwrap(),
            DeviceProviderOutcome::Saved
        );
        assert_eq!(
            store.resolve("codex-chatgpt").unwrap().1.expose(),
            token.as_bytes()
        );
        let snapshot = serde_json::to_string(&store.snapshot("device").unwrap()).unwrap();
        assert!(!snapshot.contains("account-one"));
        assert!(!snapshot.contains("codexHome"));
        assert!(!snapshot.contains(&token));
        let stored: Vec<u8> = store
            .connection
            .query_row("SELECT secret FROM providers", [], |row| row.get(0))
            .unwrap();
        assert!(!String::from_utf8(stored).unwrap().contains(&token));
        assert_eq!(
            store
                .mutate(Mutation {
                    operation: "save".into(),
                    config: config(),
                    api_key: Some("should-be-rejected".into()),
                    custom_headers: None,
                })
                .unwrap(),
            DeviceProviderOutcome::InvalidRequest
        );
        let mut other = config();
        other.protocol = DeviceProviderProtocol::Canonical;
        other.endpoint = "https://example.com/responses".into();
        assert_eq!(
            store
                .mutate(Mutation {
                    operation: "save".into(),
                    config: other,
                    api_key: None,
                    custom_headers: None,
                })
                .unwrap(),
            DeviceProviderOutcome::InvalidRequest
        );
        drop(store);
        let store = DeviceProviderStore::open(&directory).unwrap();
        assert!(store.resolve("codex-chatgpt").is_ok());
        fs::remove_file(home.join("auth.json")).unwrap();
        assert!(store.resolve("codex-chatgpt").is_err());
        drop(store);
        fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    #[ignore = "calls the real ChatGPT subscription using the device's current Codex login"]
    fn current_codex_login_live_probe() {
        let directory = std::env::temp_dir().join(format!("wwc-codex-live-{}", std::process::id()));
        let store = DeviceProviderStore::open(&directory).unwrap();
        assert_eq!(
            store
                .mutate(Mutation {
                    operation: "save".into(),
                    config: config(),
                    api_key: None,
                    custom_headers: None,
                })
                .unwrap(),
            DeviceProviderOutcome::Saved
        );
        let binding: Vec<u8> = store
            .connection
            .query_row("SELECT secret FROM providers", [], |row| row.get(0))
            .unwrap();
        let json: serde_json::Value = serde_json::from_slice(&binding).unwrap();
        assert!(json.get("codexHome").is_some());
        assert!(json.get("accessToken").is_none());
        let result = store.test_provider(
            Mutation {
                operation: "test".into(),
                config: config(),
                api_key: None,
                custom_headers: None,
            },
            "codex-live-probe",
        );
        drop(store);
        fs::remove_dir_all(directory).unwrap();
        result.expect("current Codex login must complete one native Provider probe");
    }

    #[test]
    fn refreshing_one_credential_keeps_unrelated_database_writes_available() {
        let directory =
            std::env::temp_dir().join(format!("wwc-refresh-lock-{}", std::process::id()));
        let store = DeviceProviderStore::open(&directory).unwrap();
        let mut config = config();
        config.protocol = DeviceProviderProtocol::ChatgptPlan;
        config.endpoint = crate::chatgpt_oauth::ENDPOINT.into();
        let mut record = serde_json::json!({"client_id":"oaiapp_record","subject":"private-subject","access_token":"old-access",
            "refresh_token":"old-refresh","id_token":"private-id","scope":"resource.invoke chatgpt.tokens.use.direct","expires_at":0});
        store
            .connection
            .execute(
                "INSERT INTO providers VALUES (?1,?2,?3)",
                params![
                    config.provider_id,
                    serde_json::to_string(&config).unwrap(),
                    serde_json::to_vec(&record).unwrap()
                ],
            )
            .unwrap();
        let other = DeviceProviderStore::open(&directory).unwrap();
        other
            .connection
            .busy_timeout(Duration::from_millis(50))
            .unwrap();
        let (start, ready) = std::sync::mpsc::channel();
        let writer = std::thread::spawn(move || {
            ready.recv().unwrap();
            other.connection.execute(
                "UPDATE identity SET revision=revision+1 WHERE singleton=1",
                [],
            )
        });
        let result = store
            .resolve_chatgpt_connection(&config.provider_id, |credentials| {
                start.send(()).unwrap();
                assert!(
                    writer.join().unwrap().is_ok(),
                    "OAuth network waits must not hold the shared SQLite writer lock"
                );
                record["expires_at"] = serde_json::json!(u64::MAX);
                record["access_token"] = serde_json::json!("rotated-access");
                *credentials = serde_json::from_value(record).unwrap();
                Ok(())
            })
            .unwrap();
        assert_eq!(result.1.expose(), b"rotated-access");
        drop(store);
        let store = DeviceProviderStore::open(&directory).unwrap();
        assert_eq!(
            store.resolve(&config.provider_id).unwrap().1.expose(),
            b"rotated-access"
        );
        drop(store);
        fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn refreshed_credentials_cannot_overwrite_a_replacement_authorization() {
        let directory =
            std::env::temp_dir().join(format!("wwc-refresh-cas-{}", std::process::id()));
        let store = DeviceProviderStore::open(&directory).unwrap();
        let mut config = config();
        config.protocol = DeviceProviderProtocol::ChatgptPlan;
        config.endpoint = crate::chatgpt_oauth::ENDPOINT.into();
        let record = serde_json::json!({"client_id":"oaiapp_record","subject":"old-subject","access_token":"old-access",
            "refresh_token":"old-refresh","id_token":"private-id","scope":"resource.invoke chatgpt.tokens.use.direct","expires_at":0});
        store
            .connection
            .execute(
                "INSERT INTO providers VALUES (?1,?2,?3)",
                params![
                    config.provider_id,
                    serde_json::to_string(&config).unwrap(),
                    serde_json::to_vec(&record).unwrap()
                ],
            )
            .unwrap();
        let other = DeviceProviderStore::open(&directory).unwrap();
        let result = store.resolve_chatgpt_connection(&config.provider_id, |credentials| {
            let mut replacement = record.clone();
            replacement["subject"] = serde_json::json!("new-subject");
            replacement["access_token"] = serde_json::json!("new-authorization");
            replacement["expires_at"] = serde_json::json!(u64::MAX);
            other
                .connection
                .execute(
                    "UPDATE providers SET secret=?1 WHERE provider_id=?2",
                    params![
                        serde_json::to_vec(&replacement).unwrap(),
                        config.provider_id
                    ],
                )
                .unwrap();
            let mut rotated = record;
            rotated["expires_at"] = serde_json::json!(u64::MAX);
            rotated["access_token"] = serde_json::json!("stale-rotation");
            *credentials = serde_json::from_value(rotated).unwrap();
            Ok(())
        });
        assert!(result.is_err());
        assert_eq!(
            store.resolve(&config.provider_id).unwrap().1.expose(),
            b"new-authorization"
        );
        drop(other);
        drop(store);
        fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn chatgpt_credentials_survive_restart_and_stay_out_of_public_snapshots() {
        let directory =
            std::env::temp_dir().join(format!("wwc-chatgpt-record-{}", std::process::id()));
        let mut config = config();
        config.protocol = DeviceProviderProtocol::ChatgptPlan;
        config.endpoint = crate::chatgpt_oauth::ENDPOINT.into();
        assert!(valid_device_provider_config(&config));
        let store = DeviceProviderStore::open(&directory).unwrap();
        let record = serde_json::json!({"client_id":"oaiapp_record", "subject":"private-subject", "access_token":"private-access",
            "refresh_token":"private-refresh", "id_token":"private-id", "scope":"resource.invoke chatgpt.tokens.use.direct", "expires_at":u64::MAX});
        store
            .connection
            .execute(
                "INSERT INTO providers VALUES (?1,?2,?3)",
                params![
                    config.provider_id,
                    serde_json::to_string(&config).unwrap(),
                    serde_json::to_vec(&record).unwrap()
                ],
            )
            .unwrap();
        let snapshot = serde_json::to_string(&store.snapshot("device").unwrap()).unwrap();
        for private in [
            "private-subject",
            "private-access",
            "private-refresh",
            "private-id",
            "oaiapp_record",
        ] {
            assert!(!snapshot.contains(private));
        }
        assert_eq!(
            store.resolve(&config.provider_id).unwrap().1.expose(),
            b"private-access"
        );
        assert_eq!(
            store
                .mutate(Mutation {
                    operation: "save".into(),
                    config: config.clone(),
                    api_key: None,
                    custom_headers: None,
                })
                .unwrap(),
            DeviceProviderOutcome::Saved
        );
        assert_eq!(
            store
                .mutate(Mutation {
                    operation: "authorize".into(),
                    config: config.clone(),
                    api_key: Some("api-key".into()),
                    custom_headers: None,
                })
                .unwrap(),
            DeviceProviderOutcome::InvalidRequest
        );
        let mut custom = config.clone();
        custom.endpoint = "https://other.example/responses".into();
        assert!(!valid_device_provider_config(&custom));
        custom.protocol = DeviceProviderProtocol::Canonical;
        assert_eq!(
            store
                .mutate(Mutation {
                    operation: "save".into(),
                    config: custom,
                    api_key: None,
                    custom_headers: None,
                })
                .unwrap(),
            DeviceProviderOutcome::InvalidRequest
        );
        assert_failed_authorization_preserves_credentials(&store, &config);
        drop(store);
        let store = DeviceProviderStore::open(&directory).unwrap();
        assert_eq!(
            store.resolve(&config.provider_id).unwrap().1.expose(),
            b"private-access"
        );
        drop(store);
        fs::remove_dir_all(directory).unwrap();
    }

    fn assert_failed_authorization_preserves_credentials(
        store: &DeviceProviderStore,
        config: &DeviceProviderConfig,
    ) {
        let mut receipt = DeviceProviderReceipt {
            request_id: "failed-authorization".into(),
            outcome: DeviceProviderOutcome::Interrupted,
            revision: store.revision().unwrap(),
        };
        store
            .connection
            .execute(
                "INSERT INTO receipts VALUES (?1,?2,?3)",
                params![
                    receipt.request_id,
                    "test-digest",
                    serde_json::to_string(&receipt).unwrap()
                ],
            )
            .unwrap();
        store
            .finish_authorization(
                Mutation {
                    operation: "authorize".into(),
                    config: config.clone(),
                    api_key: None,
                    custom_headers: None,
                },
                &mut receipt,
                Err(DeviceProviderError),
            )
            .unwrap();
        assert_eq!(receipt.outcome, DeviceProviderOutcome::ProviderUnavailable);
        assert_eq!(
            store.resolve(&config.provider_id).unwrap().1.expose(),
            b"private-access"
        );
        receipt.revision -= 1;
        store
            .finish_authorization(
                Mutation {
                    operation: "authorize".into(),
                    config: config.clone(),
                    api_key: None,
                    custom_headers: None,
                },
                &mut receipt,
                Err(DeviceProviderError),
            )
            .unwrap();
        assert_eq!(receipt.outcome, DeviceProviderOutcome::RevisionConflict);
        assert_eq!(
            store.resolve(&config.provider_id).unwrap().1.expose(),
            b"private-access"
        );
    }

    #[test]
    #[ignore = "opens a system browser for human consent and saves credentials in WWC_CHATGPT_LOGIN_DEVICE_DIR"]
    fn direct_chatgpt_authorization_live_probe() {
        let directory = std::env::var_os("WWC_CHATGPT_LOGIN_DEVICE_DIR")
            .expect("explicit private Device Provider directory is required");
        let store = DeviceProviderStore::open(Path::new(&directory)).unwrap();
        let mut config = config();
        config.provider_id = "chatgpt-personal".into();
        config.display_name = "ChatGPT".into();
        config.protocol = DeviceProviderProtocol::ChatgptPlan;
        config.endpoint = crate::chatgpt_oauth::ENDPOINT.into();
        let mut receipt = DeviceProviderReceipt {
            request_id: format!("chatgpt-login-{}", std::process::id()),
            outcome: DeviceProviderOutcome::Interrupted,
            revision: store.revision().unwrap(),
        };
        store
            .connection
            .execute(
                "INSERT INTO receipts VALUES (?1,?2,?3)",
                params![
                    receipt.request_id,
                    "manual-live-probe",
                    serde_json::to_string(&receipt).unwrap()
                ],
            )
            .unwrap();
        store
            .authorize_provider(
                Mutation {
                    operation: "authorize".into(),
                    config: config.clone(),
                    api_key: None,
                    custom_headers: None,
                },
                &mut receipt,
            )
            .unwrap();
        assert_eq!(
            receipt.outcome,
            DeviceProviderOutcome::Saved,
            "browser authorization must finish successfully"
        );
        let (saved, _) = store.resolve(&config.provider_id).unwrap();
        store
            .test_provider(
                Mutation {
                    operation: "test".into(),
                    config: saved,
                    api_key: None,
                    custom_headers: None,
                },
                "chatgpt-login-inference-probe",
            )
            .expect("authorized account must complete a native Provider inference");
    }

    #[test]
    #[ignore = "calls a real model using the authorized WWC_CHATGPT_LOGIN_DEVICE_DIR account"]
    fn saved_chatgpt_connection_live_probe() {
        let directory = std::env::var_os("WWC_CHATGPT_LOGIN_DEVICE_DIR")
            .expect("explicit private Device Provider directory is required");
        let store = DeviceProviderStore::open(Path::new(&directory)).unwrap();
        let (config, _) = store.resolve("chatgpt-personal").unwrap();
        assert_eq!(config.protocol, DeviceProviderProtocol::ChatgptPlan);
        store
            .test_provider(
                Mutation {
                    operation: "test".into(),
                    config,
                    api_key: None,
                    custom_headers: None,
                },
                "chatgpt-saved-connection-probe",
            )
            .expect("saved authorization must complete one native Provider inference");
    }
}
