// SPDX-License-Identifier: Apache-2.0

//! Device-private Provider settings. The server relays authenticated ciphertext only.

use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt, PermissionsExt};
use std::{fmt, fs, path::Path, time::Duration};

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
        let path = directory.join("providers.sqlite3");
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
        let transaction =
            connection.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
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
        } else if ![1, 2].contains(&version) {
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
        let mut bytes: Vec<u8> = transaction.query_row(
            "SELECT private_key FROM identity WHERE singleton=1",
            [],
            |row| row.get(0),
        )?;
        let key = SecretKey::from_slice(&bytes).map_err(|_| DeviceProviderError)?;
        bytes.fill(0);
        transaction.commit()?;
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
                    receipt.outcome = match self
                        .decrypt(envelope)
                        .and_then(|mutation| self.test_provider(mutation, &envelope.request_id))
                    {
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

    fn mutate(&self, mut mutation: Mutation) -> Result<DeviceProviderOutcome, DeviceProviderError> {
        if !valid_device_provider_config(&mutation.config) {
            return Ok(DeviceProviderOutcome::InvalidRequest);
        }
        match mutation.operation.as_str() {
            "save" => {
                let secret = match mutation.api_key.take() {
                    Some(value)
                        if !value.is_empty()
                            && value.len() <= 8192
                            && !value.chars().any(char::is_control) =>
                    {
                        ResolvedSecret::from_bytes(value.into_bytes())
                            .map_err(|_| DeviceProviderError)?
                    }
                    None => match self.resolve(&mutation.config.provider_id) {
                        Ok((_, secret)) => secret,
                        Err(_) => return Ok(DeviceProviderOutcome::InvalidRequest),
                    },
                    _ => return Ok(DeviceProviderOutcome::InvalidRequest),
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
                self.connection.execute(
                    "UPDATE identity SET revision=revision+1 WHERE singleton=1",
                    [],
                )?;
                Ok(DeviceProviderOutcome::Saved)
            }
            "test" => Ok(DeviceProviderOutcome::Interrupted),
            "delete" => {
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
        let secret = match mutation.api_key.take() {
            Some(key)
                if !key.is_empty() && key.len() <= 8192 && !key.chars().any(char::is_control) =>
            {
                ResolvedSecret::from_bytes(key.into_bytes()).map_err(|_| DeviceProviderError)?
            }
            None => self.resolve(&mutation.config.provider_id)?.1,
            _ => return Err(DeviceProviderError),
        };
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
        let adapter = adapter(&mutation.config)?;
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

    /// Reads one configured route for a device-local request.
    ///
    /// # Errors
    /// Rejects a missing or corrupt Provider. The caller must check enabled/model selection.
    pub fn resolve(
        &self,
        provider_id: &str,
    ) -> Result<(DeviceProviderConfig, ResolvedSecret), DeviceProviderError> {
        let (config, secret): (String, Vec<u8>) = self.connection.query_row(
            "SELECT config, secret FROM providers WHERE provider_id=?1",
            [provider_id],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )?;
        let secret = ResolvedSecret::from_bytes(secret).map_err(|_| DeviceProviderError)?;
        let config = serde_json::from_str(&config)?;
        if !valid_device_provider_config(&config) {
            return Err(DeviceProviderError);
        }
        Ok((config, secret))
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
}

pub(crate) fn adapter(
    config: &DeviceProviderConfig,
) -> Result<HttpsSseProviderAdapter, DeviceProviderError> {
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
    if let Some(path) = std::env::var_os(DEVICE_PROVIDER_TLS_ROOT_DER_ENVIRONMENT) {
        transport = transport
            .with_specific_tls_roots(vec![fs::read(path)?])
            .map_err(|_| DeviceProviderError)?;
    }
    if config.protocol == DeviceProviderProtocol::AnthropicMessages {
        transport = transport
            .with_anthropic_messages(8192, ProviderTokenPricing::default())
            .map_err(|_| DeviceProviderError)?;
    }
    HttpsSseProviderAdapter::try_new(transport).map_err(|_| DeviceProviderError)
}
