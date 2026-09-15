// SPDX-License-Identifier: Apache-2.0

//! Model exchanges execute on the Device, with local replay records before network effects.

use base64::{Engine as _, engine::general_purpose::STANDARD};
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
        if self.model_cancelled(&open.model_exchange_id.0)? {
            return Ok(Vec::new());
        }
        let digest = format!("{:x}", Sha256::digest(serde_json::to_vec(open)?));
        let inserted = self.connection.execute(
            "INSERT OR IGNORE INTO exchanges (exchange_id, digest) VALUES (?1, ?2)",
            params![open.model_exchange_id.0, digest],
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
        let chunks = if inserted == 0 {
            vec![model_failure(
                open,
                "DEVICE_MODEL_INTERRUPTED: previous request outcome is unknown",
            )]
        } else {
            self.invoke_model(open).unwrap_or_else(|_| {
                vec![model_failure(
                    open,
                    "DEVICE_PROVIDER_UNAVAILABLE: check this device's Provider settings",
                )]
            })
        };
        self.connection.execute(
            "UPDATE exchanges SET chunks=?1 WHERE exchange_id=?2 AND cancelled=0",
            params![serde_json::to_string(&chunks)?, open.model_exchange_id.0],
        )?;
        Ok(chunks)
    }

    fn invoke_model(
        &self,
        open: &ModelOpenMessage,
    ) -> Result<Vec<ModelChunkMessage>, DeviceProviderError> {
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
        let request: serde_json::Value = serde_json::from_slice(&payload)?;
        let provider_id = request
            .get("provider")
            .and_then(serde_json::Value::as_str)
            .ok_or(DeviceProviderError)?;
        let model_id = request
            .pointer("/request/model")
            .and_then(serde_json::Value::as_str)
            .ok_or(DeviceProviderError)?;
        let (config, secret) = self.resolve(provider_id)?;
        if !config.enabled || !config.model_ids.iter().any(|model| model == model_id) {
            return Err(DeviceProviderError);
        }
        let adapter = crate::device_store::adapter(&config)?;
        let adapter_request_id = format!("device-{}", open.model_exchange_id.0);
        let mut leak_gate = CredentialLeakGate::new();
        leak_gate.track_secret(&secret);
        if let Err(error) = adapter.open(
            &ProviderAdapterInvocation {
                model_exchange_id: &open.model_exchange_id,
                request_id: &open.request_id,
                adapter_request_id: &adapter_request_id,
                model_id,
                content_type: &open.request.content_type,
                payload: &payload,
            },
            &secret,
        ) {
            return Ok(vec![model_failure(
                open,
                match error.kind() {
                    crate::ProviderAdapterErrorKind::Rejected => "DEVICE_PROVIDER_REQUEST_REJECTED",
                    crate::ProviderAdapterErrorKind::RateLimited => "DEVICE_PROVIDER_RATE_LIMITED",
                    crate::ProviderAdapterErrorKind::Unavailable => {
                        "DEVICE_PROVIDER_CONNECTION_FAILED"
                    }
                    crate::ProviderAdapterErrorKind::Protocol => "DEVICE_PROVIDER_PROTOCOL_FAILED",
                },
            )]);
        }
        drop(secret);
        let receipt = ProviderGatewayOpenReceipt {
            model_exchange_id: open.model_exchange_id.clone(),
            request_id: open.request_id.clone(),
            route: ModelRoute {
                provider_id: provider_id.to_owned(),
                model_id: model_id.to_owned(),
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

    /// Durably fences later opens and replay after Worker cancellation.
    ///
    /// # Errors
    /// Returns a bounded storage failure.
    pub fn cancel_model(&self, exchange_id: &str) -> Result<(), DeviceProviderError> {
        self.connection.execute("INSERT INTO exchanges (exchange_id, digest, cancelled) VALUES (?1, '', 1) ON CONFLICT(exchange_id) DO UPDATE SET cancelled=1", [exchange_id])?;
        Ok(())
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

fn provider_error_message(kind: crate::HttpsSseProviderErrorKind) -> &'static str {
    use crate::HttpsSseProviderErrorKind as Kind;
    match kind {
        Kind::InvalidConfiguration => "DEVICE_PROVIDER_INVALID_CONFIGURATION",
        Kind::IdentityConflict => "DEVICE_MODEL_IDENTITY_CONFLICT",
        Kind::RateLimited => "DEVICE_PROVIDER_RATE_LIMITED",
        Kind::Rejected => "DEVICE_PROVIDER_REQUEST_REJECTED",
        Kind::Unavailable => "DEVICE_PROVIDER_UNAVAILABLE",
        Kind::Transport => "DEVICE_PROVIDER_TRANSPORT_FAILED",
        Kind::Protocol => "DEVICE_PROVIDER_PROTOCOL_FAILED",
        Kind::SizeLimit => "DEVICE_PROVIDER_RESPONSE_TOO_LARGE",
        Kind::Paused => "DEVICE_MODEL_PAUSED",
        Kind::CredentialLeak => "DEVICE_PROVIDER_CREDENTIAL_LEAK_BLOCKED",
    }
}

/// A stable terminal failure that cannot contain Provider diagnostics or credentials.
pub fn model_failure(open: &ModelOpenMessage, message: &'static str) -> ModelChunkMessage {
    let mut chunk = model_chunk(open, 1);
    chunk.error = Some(ExecutionPortError {
        code: ExecutionPortErrorCode::ModelStreamFailed,
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
