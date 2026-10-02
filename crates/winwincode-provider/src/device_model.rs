// SPDX-License-Identifier: Apache-2.0

//! Model exchanges execute on the Device, with local replay records before network effects.

use base64::{Engine as _, engine::general_purpose::STANDARD};
use std::sync::{Mutex, OnceLock};
type ActiveDeviceExchange = std::sync::Arc<crate::provider_transport::ExchangeCancellation>;
static ACTIVE_EXCHANGES: OnceLock<
    Mutex<std::collections::BTreeMap<(String, String), ActiveDeviceExchange>>,
> = OnceLock::new();
struct ExchangeRegistration((String, String));
impl Drop for ExchangeRegistration {
    fn drop(&mut self) {
        if let Ok(mut active) = ACTIVE_EXCHANGES.get_or_init(Mutex::default).lock() {
            active.remove(&self.0);
        }
    }
}

use rusqlite::{OptionalExtension, params};
use sha2::{Digest, Sha256};
use winwincode_api::generated::ModelRoute;
use winwincode_domain::{CredentialReferenceId, ExecutionMessageId, ExecutionSequence};
use winwincode_execution_port::generated::{
    ExecutionPortError, ExecutionPortErrorCode, ModelChunkMessage, ModelChunkMessageKind,
    ModelOpenMessage,
};

use crate::{
    CredentialLeakGate, DeviceProviderError, DeviceProviderStore, ProviderAdapterInvocation,
    ProviderAdapterPort, ProviderGatewayOpenReceipt, ProviderStreamControlAction,
};

enum DeviceModelFailure {
    LeaseExpired,
    RequestInvalid,
    InvalidConfiguration,
    Unavailable,
}

impl DeviceModelFailure {
    const fn code(&self) -> &'static str {
        match self {
            Self::LeaseExpired => "DEVICE_MODEL_START_LEASE_EXPIRED",
            Self::RequestInvalid => "DEVICE_PROVIDER_REQUEST_INVALID",
            Self::InvalidConfiguration => "DEVICE_PROVIDER_INVALID_CONFIGURATION",
            Self::Unavailable => "DEVICE_PROVIDER_UNAVAILABLE",
        }
    }
}

impl From<DeviceProviderError> for DeviceModelFailure {
    fn from(_: DeviceProviderError) -> Self {
        Self::Unavailable
    }
}

impl From<serde_json::Error> for DeviceModelFailure {
    fn from(_: serde_json::Error) -> Self {
        Self::RequestInvalid
    }
}

impl DeviceProviderStore {
    /// Executes a model request using this Device's configuration and secret.
    /// A recovered unfinished request fails explicitly: it is never charged a second time.
    ///
    /// # Errors
    /// Rejects conflicting exchange identities and unavailable durable storage.
    pub fn execute_model(
        &self,
        open: &ModelOpenMessage,
    ) -> Result<Vec<ModelChunkMessage>, DeviceProviderError> {
        self.execute_model_authorized(open, || true)
    }

    /// Restores exact previous exchanges, but gates every new Provider invocation
    /// on the Worker's current lease deadline, including time spent preparing input.
    ///
    /// # Errors
    /// Rejects conflicting exchange identities and unavailable durable storage.
    pub fn execute_model_authorized(
        &self,
        open: &ModelOpenMessage,
        can_start: impl Fn() -> bool,
    ) -> Result<Vec<ModelChunkMessage>, DeviceProviderError> {
        self.execute_model_with(open, &can_start, || self.invoke_model(open, &can_start))
    }

    /// Checks exact durable exchange identity without creating a first-start record.
    ///
    /// # Errors
    /// Rejects a reused identity with different content or unavailable storage.
    pub fn model_start_recorded(
        &self,
        open: &ModelOpenMessage,
    ) -> Result<bool, DeviceProviderError> {
        let stored: Option<String> = self
            .connection
            .query_row(
                "SELECT digest FROM exchanges WHERE exchange_id=?1 AND cancelled=0",
                [&open.model_exchange_id.0],
                |row| row.get(0),
            )
            .optional()?;
        let Some(stored) = stored else {
            return Ok(false);
        };
        let digest = format!("{:x}", Sha256::digest(serde_json::to_vec(open)?));
        if stored != digest {
            return Err(DeviceProviderError);
        }
        Ok(true)
    }

    fn execute_model_with(
        &self,
        open: &ModelOpenMessage,
        can_start: &impl Fn() -> bool,
        invoke: impl FnOnce() -> Result<Vec<ModelChunkMessage>, DeviceModelFailure>,
    ) -> Result<Vec<ModelChunkMessage>, DeviceProviderError> {
        if self.model_cancelled(&open.model_exchange_id.0)? {
            return Ok(Vec::new());
        }
        let key = (
            self.connection
                .path()
                .ok_or(DeviceProviderError)?
                .to_owned(),
            open.model_exchange_id.0.clone(),
        );
        let mut active = ACTIVE_EXCHANGES
            .get_or_init(Mutex::default)
            .lock()
            .map_err(|_| DeviceProviderError)?;
        let request_open = serde_json::to_string(open)?;
        let digest = format!("{:x}", Sha256::digest(request_open.as_bytes()));
        if !self.model_start_recorded(open)? && !can_start() {
            return Ok(vec![model_failure(
                open,
                DeviceModelFailure::LeaseExpired.code(),
            )]);
        }
        let inserted = self.connection.execute(
            "INSERT OR IGNORE INTO exchanges (exchange_id, digest, request_open) SELECT ?1, ?2, ?3 WHERE NOT EXISTS(SELECT 1 FROM accounting_closed_attempts WHERE job_id=?4 AND attempt=?5)",
            params![open.model_exchange_id.0, digest, request_open,open.lease.job_id.0,open.lease.attempt],
        )?;
        let (original, previous): (String, Option<String>) = self.connection.query_row(
            "SELECT digest, chunks FROM exchanges WHERE exchange_id=?1",
            [&open.model_exchange_id.0],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )?;
        if original != digest {
            return Err(DeviceProviderError);
        }
        if let Some(chunks) = previous {
            return Ok(serde_json::from_str(&chunks)?);
        }
        if inserted == 0 {
            if active.contains_key(&key) {
                // A concurrent replay must never overwrite the active call or
                // detach its cancellation authority.
                return Err(DeviceProviderError);
            }
            let chunks = vec![model_failure(open, "DEVICE_MODEL_INTERRUPTED")];
            self.connection.execute("UPDATE exchanges SET chunks=?1 WHERE exchange_id=?2 AND chunks IS NULL AND cancelled=0",params![serde_json::to_string(&chunks)?,open.model_exchange_id.0])?;
            return Ok(chunks);
        }
        let cancellation =
            std::sync::Arc::new(crate::provider_transport::ExchangeCancellation::default());
        active.insert(key.clone(), std::sync::Arc::clone(&cancellation));
        drop(active);
        let _registration = ExchangeRegistration(key);
        if self.model_cancelled(&open.model_exchange_id.0)? {
            cancellation.cancel();
            return Ok(Vec::new());
        }
        let chunks = if can_start() {
            invoke()
        } else {
            Err(DeviceModelFailure::LeaseExpired)
        }
        .unwrap_or_else(|error| vec![model_failure(open, error.code())]);
        // Paid receipts survive cancellation even when runtime replay is fenced.
        self.connection.execute("UPDATE exchanges SET accounting_chunks=?1 WHERE exchange_id=?2 AND accounting_chunks IS NULL",params![serde_json::to_string(&chunks)?,open.model_exchange_id.0])?;
        self.connection.execute(
            "UPDATE exchanges SET chunks=?1 WHERE exchange_id=?2 AND cancelled=0",
            params![serde_json::to_string(&chunks)?, open.model_exchange_id.0],
        )?;
        Ok(chunks)
    }

    #[allow(clippy::too_many_lines)]
    fn invoke_model(
        &self,
        open: &ModelOpenMessage,
        can_start: &impl Fn() -> bool,
    ) -> Result<Vec<ModelChunkMessage>, DeviceModelFailure> {
        let mut payload =
            validated_model_payload(open).map_err(|_| DeviceModelFailure::RequestInvalid)?;
        let mut request: serde_json::Value = serde_json::from_slice(&payload)?;
        let provider_id = request
            .get("provider")
            .and_then(serde_json::Value::as_str)
            .ok_or(DeviceModelFailure::RequestInvalid)?
            .to_owned();
        let model_id = request
            .pointer("/request/model")
            .and_then(serde_json::Value::as_str)
            .ok_or(DeviceModelFailure::RequestInvalid)?
            .to_owned();
        let (config, secret) = self.resolve(&provider_id)?;
        if !config.enabled || !config.model_ids.iter().any(|model| model == &model_id) {
            return Err(DeviceModelFailure::Unavailable);
        }
        if request.get("winwincodeJevContext").is_some()
            || request.get("winwincodeJevTask").is_some()
        {
            let runtime = tokio::runtime::Builder::new_current_thread()
                .enable_time()
                .build()
                .map_err(|_| DeviceProviderError)?;
            if runtime
                .block_on(self.prepare_jev_model_request(open, &mut request, can_start))
                .is_err()
            {
                if !can_start() {
                    return Err(DeviceModelFailure::LeaseExpired);
                }
                return Ok(vec![model_failure(open, "DEVICE_JEV_UNAVAILABLE")]);
            }
            payload = serde_json::to_vec(&request)?;
        }
        let adapter = crate::device_store::adapter(&config, self.custom_headers(&provider_id)?)
            .map_err(|_| DeviceModelFailure::InvalidConfiguration)?;
        let adapter_request_id = format!("device-{}", open.model_exchange_id.0);
        let adapter = match self.active_cancellation(&open.model_exchange_id.0)? {
            Some(cancellation) => adapter.with_cancellation(cancellation),
            None => return Err(DeviceModelFailure::Unavailable),
        };
        if self.model_cancelled(&open.model_exchange_id.0)? {
            let _ = adapter.control(
                &open.model_exchange_id,
                &adapter_request_id,
                ProviderStreamControlAction::Cancel,
            );
            return Ok(Vec::new());
        }
        let mut leak_gate = CredentialLeakGate::new();
        leak_gate.track_secret(&secret);
        self.retain_prepared_payload(open, &payload)?;
        if !can_start() {
            return Err(DeviceModelFailure::LeaseExpired);
        }
        if let Err(error) = adapter.open(
            &ProviderAdapterInvocation {
                model_exchange_id: &open.model_exchange_id,
                request_id: &open.request_id,
                adapter_request_id: &adapter_request_id,
                model_id: &model_id,
                content_type: &open.request.content_type,
                payload: &payload,
            },
            &secret,
        ) {
            return Ok(vec![model_failure(
                open,
                adapter_error_message(error.kind()),
            )]);
        }
        drop(secret);
        let receipt = ProviderGatewayOpenReceipt {
            model_exchange_id: open.model_exchange_id.clone(),
            request_id: open.request_id.clone(),
            route: ModelRoute {
                provider_id: provider_id.clone(),
                model_id,
                credential_reference_id: CredentialReferenceId(format!(
                    "crd_0{}",
                    &format!("{:X}", Sha256::digest(provider_id.as_bytes()))[..25]
                )),
            },
            adapter_request_id,
            idempotent_replay: false,
            stream_leak_gate: leak_gate,
        };
        let completion = adapter.drain_canonical(&receipt);
        let _ = adapter.control(
            &open.model_exchange_id,
            &receipt.adapter_request_id,
            ProviderStreamControlAction::Release,
        );
        let completion = match completion {
            Ok(completion) => completion,
            Err(error) => {
                return Ok(vec![model_failure(
                    open,
                    provider_error_message(error.kind()),
                )]);
            }
        };
        completion
            .frames
            .iter()
            .map(|frame| {
                let mut chunk = model_chunk(
                    open,
                    i64::try_from(frame.sequence()).map_err(|_| DeviceProviderError)?,
                );
                chunk.payload = Some(frame.encoded_payload());
                chunk.is_final = frame.is_terminal();
                Ok(chunk)
            })
            .collect()
    }

    // Retain the exact adapter input before any network side effect. Prepared does
    // not mean sent or accepted; terminal chunks separately describe the outcome.
    fn retain_prepared_payload(
        &self,
        open: &ModelOpenMessage,
        payload: &[u8],
    ) -> Result<(), DeviceProviderError> {
        let digest = format!("{:x}", Sha256::digest(serde_json::to_vec(open)?));
        let changed = self.connection.execute(
            "UPDATE exchanges SET prepared_payload=?1 WHERE exchange_id=?2 AND digest=?3 AND request_open IS NOT NULL AND prepared_payload IS NULL AND chunks IS NULL AND cancelled=0",
            params![payload, open.model_exchange_id.0, digest],
        )?;
        if changed != 1 {
            return Err(DeviceProviderError);
        }
        Ok(())
    }

    /// Durably fences later opens and replay after Worker cancellation.
    ///
    /// # Errors
    /// Returns a bounded storage failure.
    pub fn cancel_model(&self, exchange_id: &str) -> Result<(), DeviceProviderError> {
        self.connection.execute("INSERT INTO exchanges (exchange_id, digest, cancelled) VALUES (?1, '', 1) ON CONFLICT(exchange_id) DO UPDATE SET cancelled=1", [exchange_id])?;
        let key = (
            self.connection
                .path()
                .ok_or(DeviceProviderError)?
                .to_owned(),
            exchange_id.to_owned(),
        );
        let active = ACTIVE_EXCHANGES
            .get_or_init(Mutex::default)
            .lock()
            .map_err(|_| DeviceProviderError)?
            .get(&key)
            .cloned();
        if let Some(cancellation) = active {
            cancellation.cancel();
        }
        Ok(())
    }

    pub(crate) fn active_cancellation(
        &self,
        exchange_id: &str,
    ) -> Result<Option<ActiveDeviceExchange>, DeviceProviderError> {
        let key = (
            self.connection
                .path()
                .ok_or(DeviceProviderError)?
                .to_owned(),
            exchange_id.to_owned(),
        );
        Ok(ACTIVE_EXCHANGES
            .get_or_init(Mutex::default)
            .lock()
            .map_err(|_| DeviceProviderError)?
            .get(&key)
            .cloned())
    }
    pub(crate) fn jev_cancellation(
        &self,
        operation_id: &str,
    ) -> Result<Option<ActiveDeviceExchange>, DeviceProviderError> {
        let Some(rest) = operation_id
            .strip_prefix("jev:")
            .or_else(|| operation_id.strip_prefix("judge:"))
        else {
            return Ok(None);
        };
        let Some((exchange, _)) = rest.rsplit_once(':') else {
            return Ok(None);
        };
        self.active_cancellation(exchange)
    }

    /// Checks the durable cancellation fence.
    ///
    /// # Errors
    /// Returns a bounded storage failure.
    pub fn model_cancelled(&self, exchange_id: &str) -> Result<bool, DeviceProviderError> {
        Ok(self
            .connection
            .query_row(
                "SELECT cancelled FROM exchanges WHERE exchange_id=?1",
                [exchange_id],
                |row| row.get(0),
            )
            .optional()?
            .unwrap_or(false))
    }

    /// Durably stores Provider response chunks for one exchange identity.
    ///
    /// Used by recovery paths and tests that must reconstruct the exact
    /// Device-side replay ledger without re-invoking a Provider adapter.
    ///
    /// # Errors
    /// Rejects conflicting exchange identities and unavailable storage.
    pub fn retain_stored_model_exchange_chunks(
        &self,
        exchange_id: &str,
        chunks: &[ModelChunkMessage],
    ) -> Result<(), DeviceProviderError> {
        if exchange_id.trim().is_empty() {
            return Err(DeviceProviderError);
        }
        let payload = serde_json::to_string(chunks)?;
        self.connection.execute(
            "INSERT INTO exchanges (exchange_id, digest, chunks, cancelled)
             VALUES (?1, 'retained', ?2, 0)
             ON CONFLICT(exchange_id) DO UPDATE SET chunks = excluded.chunks
             WHERE exchanges.cancelled = 0",
            rusqlite::params![exchange_id, payload],
        )?;
        Ok(())
    }

    /// Lists exchange identities that currently store Provider response chunks.
    ///
    /// # Errors
    /// Rejects unavailable durable storage.
    pub fn list_stored_model_exchanges(&self) -> Result<Vec<String>, DeviceProviderError> {
        let mut statement = self.connection.prepare(
            "SELECT exchange_id FROM exchanges WHERE cancelled = 0 AND chunks IS NOT NULL AND chunks != '' ORDER BY exchange_id",
        )?;
        let rows = statement.query_map([], |row| row.get::<_, String>(0))?;
        let mut exchanges = Vec::new();
        for row in rows {
            exchanges.push(row?);
        }
        Ok(exchanges)
    }

    /// Reads a local replay from a requested sequence, without making a network request.
    ///
    /// # Errors
    /// Rejects invalid persisted data.
    pub fn replay_model(
        &self,
        exchange_id: &str,
        from: i64,
    ) -> Result<Vec<ModelChunkMessage>, DeviceProviderError> {
        let chunks: Option<Option<String>> = self
            .connection
            .query_row(
                "SELECT chunks FROM exchanges WHERE exchange_id=?1 AND cancelled=0",
                [exchange_id],
                |row| row.get(0),
            )
            .optional()?;
        let Some(Some(chunks)) = chunks else {
            return Ok(Vec::new());
        };
        let chunks: Vec<ModelChunkMessage> = serde_json::from_str(&chunks)?;
        Ok(chunks
            .into_iter()
            .filter(|chunk| chunk.sequence.0 >= from)
            .collect())
    }
}

pub(crate) fn validated_model_payload(
    open: &ModelOpenMessage,
) -> Result<Vec<u8>, DeviceProviderError> {
    if open.request.data_base64.len() > 24 * 1024 * 1024
        || open.request.content_type != "application/json"
    {
        return Err(DeviceProviderError);
    }
    let payload = STANDARD
        .decode(&open.request.data_base64)
        .map_err(|_| DeviceProviderError)?;
    if format!("sha256:{:x}", Sha256::digest(&payload)) != open.request.payload_digest.0 {
        return Err(DeviceProviderError);
    }
    Ok(payload)
}

fn adapter_error_message(kind: crate::ProviderAdapterErrorKind) -> &'static str {
    use crate::ProviderAdapterErrorKind as Kind;
    match kind {
        Kind::RequestInvalid => "DEVICE_PROVIDER_REQUEST_INVALID",
        Kind::RequestTranslation => "DEVICE_PROVIDER_REQUEST_TRANSLATION_FAILED",
        Kind::RequestSizeLimit => "DEVICE_PROVIDER_REQUEST_TOO_LARGE",
        Kind::ResponseContentType => "DEVICE_PROVIDER_RESPONSE_CONTENT_TYPE_INVALID",
        Kind::Connection => "DEVICE_PROVIDER_CONNECTION_FAILED",
        Kind::Upstream => "DEVICE_PROVIDER_UPSTREAM_FAILED",
        Kind::IdentityConflict => "DEVICE_MODEL_IDENTITY_CONFLICT",
        Kind::Rejected => "DEVICE_PROVIDER_REQUEST_REJECTED",
        Kind::RateLimited => "DEVICE_PROVIDER_RATE_LIMITED",
        Kind::Unavailable => "DEVICE_PROVIDER_UNAVAILABLE",
        Kind::Protocol => "DEVICE_PROVIDER_ADAPTER_PROTOCOL_FAILED",
    }
}

fn provider_error_message(kind: crate::HttpsSseProviderErrorKind) -> &'static str {
    use crate::HttpsSseProviderErrorKind as Kind;
    match kind {
        Kind::InvalidConfiguration => "DEVICE_PROVIDER_INVALID_CONFIGURATION",
        Kind::IdentityConflict => "DEVICE_MODEL_IDENTITY_CONFLICT",
        Kind::RateLimited => "DEVICE_PROVIDER_RATE_LIMITED",
        Kind::Rejected => "DEVICE_PROVIDER_REQUEST_REJECTED",
        Kind::Unavailable => "DEVICE_PROVIDER_UNAVAILABLE",
        Kind::Transport => "DEVICE_PROVIDER_TRANSPORT_FAILED",
        Kind::SseFraming => "DEVICE_PROVIDER_SSE_FRAMING_INVALID",
        Kind::SseEvent => "DEVICE_PROVIDER_SSE_EVENT_INVALID",
        Kind::IncompleteStream => "DEVICE_PROVIDER_RESPONSE_INCOMPLETE",
        Kind::StreamConversion => "DEVICE_PROVIDER_STREAM_CONVERSION_FAILED",
        Kind::SizeLimit => "DEVICE_PROVIDER_RESPONSE_TOO_LARGE",
        Kind::Paused => "DEVICE_MODEL_PAUSED",
        Kind::CredentialLeak => "DEVICE_PROVIDER_CREDENTIAL_LEAK_BLOCKED",
    }
}

/// A stable terminal failure that cannot contain Provider diagnostics or credentials.
pub fn model_failure(open: &ModelOpenMessage, message: &'static str) -> ModelChunkMessage {
    let mut chunk = model_chunk(open, 1);
    chunk.error = Some(ExecutionPortError {
        code: if message == "DEVICE_MODEL_START_LEASE_EXPIRED" {
            ExecutionPortErrorCode::LeaseExpired
        } else {
            serde_json::from_value(serde_json::Value::String(message.to_owned()))
                .unwrap_or(ExecutionPortErrorCode::ModelStreamFailed)
        },
        message: message.to_owned(),
        retryable: false,
    });
    chunk.is_final = true;
    chunk
}

fn model_chunk(open: &ModelOpenMessage, sequence: i64) -> ModelChunkMessage {
    let digest = format!(
        "{:x}",
        Sha256::digest(format!("{}:{sequence}", open.model_exchange_id.0))
    );
    ModelChunkMessage {
        error: None,
        is_final: false,
        kind: ModelChunkMessageKind::ModelChunk,
        lease: open.lease.clone(),
        message_id: ExecutionMessageId(format!("xmsg_0{}", digest[..25].to_uppercase())),
        model_exchange_id: open.model_exchange_id.clone(),
        payload: None,
        schema_version: open.schema_version.clone(),
        sent_at: open.sent_at.clone(),
        sequence: ExecutionSequence(sequence),
        session_identity: open.session_identity.clone(),
        worker_session_id: open.worker_session_id.clone(),
    }
}

/// Keeps the ordered public text projection; provider events, reasoning, and tool payloads stay local.
///
/// # Errors
/// Rejects malformed stored payloads and mismatched digests.
pub fn public_model_chunk(
    chunk: &ModelChunkMessage,
) -> Result<ModelChunkMessage, DeviceProviderError> {
    let mut public = chunk.clone();
    public.payload = None;
    public.error = None;
    public.message_id = ExecutionMessageId(format!(
        "xmsg_0{}",
        &format!(
            "{:X}",
            Sha256::digest(format!("public:{}", chunk.message_id.0))
        )[..25]
    ));
    if chunk.payload.is_none()
        && let Some(error) = &chunk.error
    {
        let text = match error.message.as_str() {
            "DEVICE_PROVIDER_RATE_LIMITED" => {
                "模型服务触发速率或额度限制，请检查服务商用量后重试。"
            }
            "DEVICE_PROVIDER_REQUEST_REJECTED" => {
                "模型服务拒绝了请求，请检查所选设备的模型配置和访问权限。"
            }
            "DEVICE_PROVIDER_CONNECTION_FAILED" | "DEVICE_PROVIDER_TRANSPORT_FAILED" => {
                "设备连接模型服务失败，请检查设备网络后重试。"
            }
            _ => "模型调用未完成，请检查所选设备的服务商设置。",
        };
        let bytes =
            serde_json::to_vec(&serde_json::json!({"type":"output_text_delta", "delta":text}))?;
        public.payload = Some(winwincode_execution_port::generated::EncodedPayload {
            content_type: "application/json".to_owned(),
            data_base64: STANDARD.encode(&bytes),
            payload_digest: winwincode_domain::Sha256Digest(format!(
                "sha256:{:x}",
                Sha256::digest(&bytes)
            )),
        });
    }
    if let Some(payload) = &chunk.payload {
        let bytes = STANDARD
            .decode(&payload.data_base64)
            .map_err(|_| DeviceProviderError)?;
        if payload.content_type != "application/json"
            || payload.payload_digest.0 != format!("sha256:{:x}", Sha256::digest(&bytes))
        {
            return Err(DeviceProviderError);
        }
        let value: serde_json::Value = serde_json::from_slice(&bytes)?;
        if value.get("type").and_then(serde_json::Value::as_str) == Some("output_text_delta") {
            let delta = value
                .get("delta")
                .and_then(serde_json::Value::as_str)
                .ok_or(DeviceProviderError)?;
            let bytes = serde_json::to_vec(
                &serde_json::json!({"type":"output_text_delta", "delta":delta}),
            )?;
            public.payload = Some(winwincode_execution_port::generated::EncodedPayload {
                content_type: "application/json".to_owned(),
                data_base64: STANDARD.encode(&bytes),
                payload_digest: winwincode_domain::Sha256Digest(format!(
                    "sha256:{:x}",
                    Sha256::digest(&bytes)
                )),
            });
        }
    }
    Ok(public)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn invalid_local_model_requests_retain_a_request_failure_before_provider_resolution() {
        let directory =
            std::env::temp_dir().join(format!("wwc-invalid-model-request-{}", std::process::id()));
        let store = DeviceProviderStore::open(&directory).unwrap();
        let fixture: serde_json::Value = serde_json::from_str(include_str!(
            "../../../tests/fixtures/contracts/execution-port.valid.json"
        ))
        .unwrap();
        let original: ModelOpenMessage = serde_json::from_value(
            fixture["messages"]
                .as_array()
                .unwrap()
                .iter()
                .find(|message| message["kind"] == "model.open")
                .unwrap()
                .clone(),
        )
        .unwrap();
        for ordinal in 1..=2 {
            let mut open = original.clone();
            open.model_exchange_id.0 = format!("mdl_{ordinal:026}");
            if ordinal == 1 {
                open.request.content_type = "text/plain".into();
            } else {
                open.request.content_type = "application/json".into();
                open.request.data_base64 = STANDARD.encode(b"{}");
                open.request.payload_digest.0 = format!("sha256:{:x}", Sha256::digest(b"{}"));
            }
            let chunks = store.execute_model(&open).unwrap();
            assert_eq!(chunks.len(), 1);
            assert_eq!(
                chunks[0].error.as_ref().unwrap().code,
                ExecutionPortErrorCode::DeviceProviderRequestInvalid
            );
            assert_eq!(
                store.execute_model(&open).unwrap(),
                chunks,
                "stored failures must replay without a Provider call"
            );
        }
        drop(store);
        std::fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn first_invocation_is_gated_but_exact_recovery_never_calls_again() {
        use std::cell::Cell;
        let root = std::env::temp_dir().join(format!(
            "wwc-first-model-authority-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let store = DeviceProviderStore::open(&root).unwrap();
        let fixture: serde_json::Value = serde_json::from_str(include_str!(
            "../../../tests/fixtures/contracts/execution-port.valid.json"
        ))
        .unwrap();
        let open: ModelOpenMessage = serde_json::from_value(
            fixture["messages"]
                .as_array()
                .unwrap()
                .iter()
                .find(|message| message["kind"] == "model.open")
                .unwrap()
                .clone(),
        )
        .unwrap();
        let calls = Cell::new(0);
        let invoke = || {
            calls.set(calls.get() + 1);
            let mut chunk = model_chunk(&open, 1);
            chunk.is_final = true;
            Ok(vec![chunk])
        };
        let refused = store.execute_model_with(&open, &|| false, invoke).unwrap();
        assert_eq!(
            refused[0].error.as_ref().unwrap().code,
            ExecutionPortErrorCode::LeaseExpired
        );
        assert_eq!(calls.get(), 0);
        assert!(!store.model_start_recorded(&open).unwrap());
        // The same identity becomes eligible after a legitimate Worker renewal.
        let completed = store.execute_model_with(&open, &|| true, invoke).unwrap();
        assert_eq!(calls.get(), 1);
        assert_eq!(
            store.execute_model_with(&open, &|| false, invoke).unwrap(),
            completed
        );
        assert_eq!(
            calls.get(),
            1,
            "expired recovery restores facts without another invocation"
        );
        let mut changed = open.clone();
        changed.request_id.0.push('X');
        assert!(store.model_start_recorded(&changed).is_err());
        assert!(
            store
                .execute_model_with(&changed, &|| true, invoke)
                .is_err()
        );
        assert_eq!(calls.get(), 1);
        // Expiry after durable admission still prevents the first network invocation.
        let mut delayed = open.clone();
        delayed.model_exchange_id.0.push('Y');
        let checks = Cell::new(0);
        let delayed_result = store
            .execute_model_with(
                &delayed,
                &|| {
                    checks.set(checks.get() + 1);
                    checks.get() == 1
                },
                invoke,
            )
            .unwrap();
        assert_eq!(
            delayed_result[0].error.as_ref().unwrap().code,
            ExecutionPortErrorCode::LeaseExpired
        );
        assert_eq!(calls.get(), 1);
        assert_eq!(
            store
                .execute_model_with(&delayed, &|| false, invoke)
                .unwrap(),
            delayed_result
        );
        drop(store);
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn expiry_during_preparation_stops_before_the_real_adapter_open() {
        use std::{
            cell::Cell,
            io::ErrorKind,
            net::TcpListener,
            sync::{
                Arc,
                atomic::{AtomicBool, AtomicUsize, Ordering},
            },
        };
        let root = std::env::temp_dir().join(format!(
            "wwc-model-preparation-expiry-{}",
            std::process::id()
        ));
        let store = DeviceProviderStore::open(&root).unwrap();
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        listener.set_nonblocking(true).unwrap();
        let port = listener.local_addr().unwrap().port();
        let calls = Arc::new(AtomicUsize::new(0));
        let stopped = Arc::new(AtomicBool::new(false));
        let counted = Arc::clone(&calls);
        let stop = Arc::clone(&stopped);
        let server = std::thread::spawn(move || {
            while !stop.load(Ordering::SeqCst) {
                match listener.accept() {
                    Ok(_) => {
                        counted.fetch_add(1, Ordering::SeqCst);
                    }
                    Err(error) if error.kind() == ErrorKind::WouldBlock => {
                        std::thread::sleep(std::time::Duration::from_millis(1));
                    }
                    Err(error) => panic!("fixture accept failed: {error}"),
                }
            }
        });
        let config = serde_json::json!({"providerId":"lease-fixture","displayName":"fixture","endpoint":format!("https://127.0.0.1:{port}/v1/responses"),"protocol":"canonical","modelIds":["fixture-model"],"enabled":true});
        store
            .connection
            .execute(
                "INSERT INTO providers VALUES (?1,?2,?3)",
                params![
                    "lease-fixture",
                    config.to_string(),
                    b"fixture-only-key".as_slice()
                ],
            )
            .unwrap();
        let fixture: serde_json::Value = serde_json::from_str(include_str!(
            "../../../tests/fixtures/contracts/execution-port.valid.json"
        ))
        .unwrap();
        let mut open: ModelOpenMessage = serde_json::from_value(
            fixture["messages"]
                .as_array()
                .unwrap()
                .iter()
                .find(|message| message["kind"] == "model.open")
                .unwrap()
                .clone(),
        )
        .unwrap();
        let payload = serde_json::to_vec(&serde_json::json!({"provider":"lease-fixture","request":{"model":"fixture-model","stream":true,"input":"fixture"}})).unwrap();
        open.request.content_type = "application/json".into();
        open.request.data_base64 = STANDARD.encode(&payload);
        open.request.payload_digest.0 = format!("sha256:{:x}", Sha256::digest(&payload));
        let checks = Cell::new(0);
        let result = store
            .execute_model_authorized(&open, || {
                checks.set(checks.get() + 1);
                checks.get() < 3
            })
            .unwrap();
        stopped.store(true, Ordering::SeqCst);
        server.join().unwrap();
        assert_eq!(
            checks.get(),
            3,
            "expiry is rechecked after durable payload preparation"
        );
        assert_eq!(
            result[0].error.as_ref().unwrap().code,
            ExecutionPortErrorCode::LeaseExpired
        );
        assert_eq!(
            calls.load(Ordering::SeqCst),
            0,
            "expired first start never reaches local HTTP fixture"
        );
        let prepared: Option<Vec<u8>> = store
            .connection
            .query_row(
                "SELECT prepared_payload FROM exchanges WHERE exchange_id=?1",
                [&open.model_exchange_id.0],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(prepared, Some(payload));
        drop(store);
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn model_failure_codes_preserve_the_provider_failure_boundary() {
        let fixture: serde_json::Value = serde_json::from_str(include_str!(
            "../../../tests/fixtures/contracts/execution-port.valid.json"
        ))
        .unwrap();
        let open: ModelOpenMessage = serde_json::from_value(
            fixture["messages"]
                .as_array()
                .unwrap()
                .iter()
                .find(|message| message["kind"] == "model.open")
                .unwrap()
                .clone(),
        )
        .unwrap();
        for code in [
            "DEVICE_PROVIDER_REQUEST_INVALID",
            "DEVICE_PROVIDER_REQUEST_TRANSLATION_FAILED",
            "DEVICE_PROVIDER_REQUEST_TOO_LARGE",
            "DEVICE_PROVIDER_RESPONSE_CONTENT_TYPE_INVALID",
            "DEVICE_PROVIDER_SSE_FRAMING_INVALID",
            "DEVICE_PROVIDER_SSE_EVENT_INVALID",
            "DEVICE_PROVIDER_RESPONSE_INCOMPLETE",
            "DEVICE_PROVIDER_STREAM_CONVERSION_FAILED",
            "DEVICE_PROVIDER_CONNECTION_FAILED",
            "DEVICE_PROVIDER_UPSTREAM_FAILED",
        ] {
            let chunk = model_failure(&open, code);
            let value = serde_json::to_value(&chunk).unwrap();
            assert_eq!(value["error"]["code"], code);
            assert_eq!(value["error"]["message"], code);
            assert!(chunk.is_final);
            assert!(!chunk.error.unwrap().retryable);
        }
        assert_eq!(
            model_failure(&open, "DEVICE_PROVIDER_PROTOCOL_FAILED")
                .error
                .unwrap()
                .code,
            ExecutionPortErrorCode::ModelStreamFailed,
            "legacy unclassified failures must retain their original category"
        );
    }

    #[test]
    fn prepared_payload_is_exact_single_write_and_fenced() {
        let directory =
            std::env::temp_dir().join(format!("wwc-prepared-model-{}", std::process::id()));
        let store = DeviceProviderStore::open(&directory).unwrap();
        let fixture: serde_json::Value = serde_json::from_str(include_str!(
            "../../../tests/fixtures/contracts/execution-port.valid.json"
        ))
        .unwrap();
        let open: ModelOpenMessage = serde_json::from_value(
            fixture["messages"]
                .as_array()
                .unwrap()
                .iter()
                .find(|message| message["kind"] == "model.open")
                .unwrap()
                .clone(),
        )
        .unwrap();
        let original = serde_json::to_string(&open).unwrap();
        let digest = format!("{:x}", Sha256::digest(original.as_bytes()));
        store
            .connection
            .execute(
                "INSERT INTO exchanges (exchange_id, digest, request_open) VALUES (?1, ?2, ?3)",
                params![open.model_exchange_id.0, digest, original],
            )
            .unwrap();
        let mut changed = open.clone();
        changed.request_id.0.push('X');
        assert!(store.retain_prepared_payload(&changed, b"wrong").is_err());
        let prepared = b"{\n  \"request\": {\"input\": []}\n}";
        store.retain_prepared_payload(&open, prepared).unwrap();
        assert!(store.retain_prepared_payload(&open, b"overwrite").is_err());
        drop(store);
        let store = DeviceProviderStore::open(&directory).unwrap();
        let saved: Vec<u8> = store
            .connection
            .query_row(
                "SELECT prepared_payload FROM exchanges WHERE exchange_id=?1",
                [&open.model_exchange_id.0],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(saved, prepared);
        assert!(store.retain_prepared_payload(&open, prepared).is_err());
        for (cancelled, chunks) in [(1, None), (0, Some("[]"))] {
            store
                .connection
                .execute(
                    "UPDATE exchanges SET prepared_payload=NULL, cancelled=?1, chunks=?2",
                    params![cancelled, chunks],
                )
                .unwrap();
            assert!(store.retain_prepared_payload(&open, prepared).is_err());
        }
        drop(store);
        std::fs::remove_dir_all(directory).unwrap();
    }
}
