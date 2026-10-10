// SPDX-License-Identifier: Apache-2.0

//! Model exchanges execute on the Device, with local replay records before network effects.

#[cfg(test)]
#[path = "device_model_recovery_tests.rs"]
mod recovery_tests;

#[cfg(test)]
#[path = "device_model_deferred_regression_tests.rs"]
mod deferred_regression_tests;

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

#[derive(Debug)]
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
impl From<rusqlite::Error> for DeviceModelFailure {
    fn from(_: rusqlite::Error) -> Self {
        Self::Unavailable
    }
}

struct AdmissionRelease<'a, F: Fn()>(&'a F);
impl<F: Fn()> Drop for AdmissionRelease<'_, F> {
    fn drop(&mut self) {
        (self.0)();
    }
}

enum ModelAttemptOutcome {
    Completed(Vec<ModelChunkMessage>),
    TerminalFailure {
        chunks: Vec<ModelChunkMessage>,
        accounting: Option<Box<ModelChunkMessage>>,
        network: winwincode_network::NetworkFailure,
    },
    Failed {
        chunk: Box<ModelChunkMessage>,
        retryable: bool,
        response: Option<Vec<u8>>,
        accounting: Option<Box<ModelChunkMessage>>,
        not_sent: bool,
        network: winwincode_network::NetworkFailure,
    },
}

fn completion_outcome(
    open: &ModelOpenMessage,
    completion: &crate::HttpsSseProviderCompletion,
) -> Result<ModelAttemptOutcome, DeviceModelFailure> {
    let chunks = completion
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
        .collect::<Result<Vec<_>, DeviceProviderError>>()?;
    if completion.terminal == crate::ProviderGatewayTerminal::Cancelled {
        return Ok(ModelAttemptOutcome::TerminalFailure {
            chunks,
            accounting: None,
            network: winwincode_network::NetworkFailure::new(
                winwincode_network::ErrorKind::Cancelled,
                winwincode_network::Acceptance::ResponseReceived,
                winwincode_network::Phase::Stream,
            ),
        });
    }
    if let crate::ProviderGatewayTerminal::Failed { failure, charge } = completion.terminal {
        let mut accounting = chunks.last().cloned();
        if let Some(charge) = charge
            && let Some(chunk) = accounting.as_mut()
        {
            let mut value: serde_json::Value = serde_json::from_str(
                completion
                    .frames
                    .last()
                    .ok_or(DeviceProviderError)?
                    .payload_json(),
            )?;
            if let Some(cost) = charge.actual_cost_micros {
                value
                    .as_object_mut()
                    .ok_or(DeviceProviderError)?
                    .insert("actualCostMicros".to_owned(), cost.into());
            }
            chunk.payload = Some(encoded_json(&value)?);
        }
        let value: serde_json::Value = serde_json::from_str(
            completion
                .frames
                .last()
                .ok_or(DeviceProviderError)?
                .payload_json(),
        )?;
        let error = value.get("error").ok_or(DeviceProviderError)?;
        let metadata = crate::ProviderFailureMetadata {
            status: error
                .get("status")
                .and_then(serde_json::Value::as_u64)
                .and_then(|status| u16::try_from(status).ok()),
            provider_retry_after_millis: error
                .get("providerRetryAfterMillis")
                .and_then(serde_json::Value::as_u64),
            provider_request_id: error
                .get("providerRequestId")
                .and_then(serde_json::Value::as_str)
                .map(str::to_owned),
            diagnostic: None,
        };
        let network = completion_failure_network(failure.kind, &metadata);
        let (code, retryable) = match failure.kind {
            crate::ModelAttemptFailureKind::RateLimit => ("DEVICE_PROVIDER_RATE_LIMITED", true),
            crate::ModelAttemptFailureKind::Server => ("DEVICE_PROVIDER_UPSTREAM_FAILED", true),
            crate::ModelAttemptFailureKind::Timeout | crate::ModelAttemptFailureKind::Transport => {
                ("DEVICE_PROVIDER_TRANSPORT_FAILED", true)
            }
            crate::ModelAttemptFailureKind::Protocol => ("DEVICE_PROVIDER_PROTOCOL_FAILED", true),
            _ => {
                return Ok(ModelAttemptOutcome::TerminalFailure {
                    chunks,
                    accounting: accounting.map(Box::new),
                    network,
                });
            }
        };
        return Ok(ModelAttemptOutcome::Failed {
            chunk: Box::new(model_failure_metadata(open, code, retryable, &metadata)?),
            retryable,
            response: None,
            accounting: accounting.map(Box::new),
            not_sent: false,
            network,
        });
    }
    Ok(ModelAttemptOutcome::Completed(chunks))
}

fn completion_failure_network(
    kind: crate::ModelAttemptFailureKind,
    metadata: &crate::ProviderFailureMetadata,
) -> winwincode_network::NetworkFailure {
    use winwincode_network::{Acceptance, ErrorKind, NetworkFailure, Phase};
    let kind = match kind {
        crate::ModelAttemptFailureKind::Authentication => ErrorKind::Authentication,
        crate::ModelAttemptFailureKind::RateLimit => ErrorKind::RateLimited,
        crate::ModelAttemptFailureKind::Timeout => ErrorKind::Timeout,
        crate::ModelAttemptFailureKind::Transport => ErrorKind::TransportInterrupted,
        crate::ModelAttemptFailureKind::Server => ErrorKind::ServerTransient,
        crate::ModelAttemptFailureKind::Protocol => ErrorKind::ProtocolInvalid,
        crate::ModelAttemptFailureKind::Cancelled => ErrorKind::Cancelled,
        crate::ModelAttemptFailureKind::InvalidRequest
        | crate::ModelAttemptFailureKind::Quota
        | crate::ModelAttemptFailureKind::ContextWindowExceeded
        | crate::ModelAttemptFailureKind::ProviderUnavailable
        | crate::ModelAttemptFailureKind::Unknown => ErrorKind::RequestInvalid,
    };
    NetworkFailure {
        http_status: metadata.status,
        retry_after_ms: metadata.provider_retry_after_millis,
        ..NetworkFailure::new(kind, Acceptance::ResponseReceived, Phase::Stream)
    }
}

fn observed_accounting_chunk(
    open: &ModelOpenMessage,
    response_id: &str,
    usage: crate::ProviderTokenUsage,
) -> Result<ModelChunkMessage, DeviceProviderError> {
    let mut chunk = model_chunk(open, 1);
    chunk.is_final = true;
    chunk.payload = Some(encoded_json(
        &serde_json::json!({"type":"failed","responseId":response_id,"tokenUsage":{"inputTokens":usage.input_tokens,"outputTokens":usage.output_tokens,"cachedInputTokens":usage.cached_input_tokens,"cacheWriteInputTokens":usage.cache_write_input_tokens,"reasoningOutputTokens":usage.reasoning_output_tokens}}),
    )?);
    Ok(chunk)
}

fn encoded_json(
    value: &serde_json::Value,
) -> Result<winwincode_execution_port::generated::EncodedPayload, DeviceProviderError> {
    let bytes = serde_json::to_vec(value)?;
    Ok(winwincode_execution_port::generated::EncodedPayload {
        content_type: "application/json".to_owned(),
        data_base64: STANDARD.encode(&bytes),
        payload_digest: winwincode_domain::Sha256Digest(format!(
            "sha256:{:x}",
            Sha256::digest(&bytes)
        )),
    })
}

fn model_failure_metadata(
    open: &ModelOpenMessage,
    code: &'static str,
    retryable: bool,
    metadata: &crate::ProviderFailureMetadata,
) -> Result<ModelChunkMessage, DeviceProviderError> {
    let mut chunk = model_failure(open, code);
    chunk.error.as_mut().ok_or(DeviceProviderError)?.retryable = retryable;
    let mut value = serde_json::to_value(metadata)?;
    value.as_object_mut().ok_or(DeviceProviderError)?.insert(
        "providerErrorKind".to_owned(),
        serde_json::Value::String(
            code.strip_prefix("DEVICE_PROVIDER_")
                .unwrap_or(code)
                .to_ascii_lowercase(),
        ),
    );
    chunk.payload = Some(encoded_json(&value)?);
    Ok(chunk)
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
        self.execute_model_with(open, &can_start, || {
            self.invoke_model(open, &can_start, false, &|| Ok(()), &|| {})
        })
    }

    /// Recovers transient failures within this logical model step. Each actual
    /// invocation has a separate durable receipt; terminal exchange replay never
    /// opens a new network request. Admission is released before every wait.
    ///
    /// # Errors
    /// Rejects changed exchange identities or unavailable durable storage.
    pub fn execute_model_recovering_authorized(
        &self,
        open: &ModelOpenMessage,
        can_start: impl Fn() -> bool,
        before_attempt: impl Fn() -> Result<(), DeviceProviderError>,
        after_attempt: impl Fn(),
    ) -> Result<Vec<ModelChunkMessage>, DeviceProviderError> {
        self.execute_model_with(open, &can_start, || {
            self.invoke_model(open, &can_start, true, &before_attempt, &after_attempt)
        })
    }

    /// Retains a bounded terminal failure when Provider admission storage is unavailable.
    /// This never invokes the Provider; exact replays recover the same failure.
    ///
    /// # Errors
    /// Rejects conflicting exchange identities and unavailable durable storage.
    pub fn reject_model_start(
        &self,
        open: &ModelOpenMessage,
    ) -> Result<Vec<ModelChunkMessage>, DeviceProviderError> {
        self.execute_model_with(open, &|| true, || {
            Ok(vec![model_failure(open, "DEVICE_PROVIDER_UNAVAILABLE")])
        })
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
            self.settle_abandoned_model_accounting(&open.model_exchange_id.0)?;
            return Ok(Vec::new());
        }
        let key = (
            self.connection
                .path()
                .ok_or(DeviceProviderError)?
                .to_owned(),
            open.model_exchange_id.0.clone(),
        );
        let _owner = self
            .try_model_exchange_owner(&open.model_exchange_id.0)?
            .ok_or(DeviceProviderError)?;
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
            if !active.contains_key(&key) {
                self.settle_interrupted_invocations(open)?;
            }
            return Ok(serde_json::from_str(&chunks)?);
        }
        if inserted == 0 {
            if active.contains_key(&key) {
                // A concurrent replay must never overwrite the active call or
                // detach its cancellation authority.
                return Err(DeviceProviderError);
            }
            let chunks = vec![model_failure(open, "DEVICE_MODEL_INTERRUPTED")];
            self.settle_interrupted_invocations(open)?;
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
        recover: bool,
        before_attempt: &impl Fn() -> Result<(), DeviceProviderError>,
        after_attempt: &impl Fn(),
    ) -> Result<Vec<ModelChunkMessage>, DeviceModelFailure> {
        self.invoke_model_using(
            open,
            can_start,
            recover,
            before_attempt,
            after_attempt,
            crate::device_store::adapter,
        )
    }

    #[cfg(test)]
    pub(crate) fn execute_model_using(
        &self,
        open: &ModelOpenMessage,
        can_start: impl Fn() -> bool,
        factory: impl Fn(
            &winwincode_api::generated::DeviceProviderConfig,
            std::collections::BTreeMap<String, String>,
        ) -> Result<crate::HttpsSseProviderAdapter, DeviceProviderError>,
    ) -> Result<Vec<ModelChunkMessage>, DeviceProviderError> {
        self.execute_model_with(open, &can_start, || {
            self.invoke_model_using(
                open,
                &can_start,
                true,
                &|| Ok(()),
                &|| {},
                |config, headers, _account| factory(config, headers),
            )
        })
    }

    #[allow(clippy::too_many_lines)]
    fn invoke_model_using(
        &self,
        open: &ModelOpenMessage,
        can_start: &impl Fn() -> bool,
        recover: bool,
        before_attempt: &impl Fn() -> Result<(), DeviceProviderError>,
        after_attempt: &impl Fn(),
        make_adapter: impl Fn(
            &winwincode_api::generated::DeviceProviderConfig,
            std::collections::BTreeMap<String, String>,
            Option<String>,
        ) -> Result<crate::HttpsSseProviderAdapter, DeviceProviderError>,
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
        let (config, secret, codex_account) = self.resolve_connection(&provider_id)?;
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
                    self.retain_model_stop_reason(open, "authority_ended")?;
                    return Err(DeviceModelFailure::LeaseExpired);
                }
                return Ok(vec![model_failure(open, "DEVICE_JEV_UNAVAILABLE")]);
            }
            payload = serde_json::to_vec(&request)?;
        }
        let headers = self.custom_headers(&provider_id)?;
        self.retain_prepared_payload(open, &payload)?;
        let mut attempt_number = 0_i64;
        let mut retry =
            winwincode_network::RequestRetry::new(4, open.model_exchange_id.0.as_bytes()).state();
        let sse_failure_log = self.sse_failure_log_directory()?;
        loop {
            if self.model_cancelled(&open.model_exchange_id.0)? {
                self.retain_model_stop_reason(open, "authority_ended")?;
                return Ok(Vec::new());
            }
            if !can_start() {
                self.retain_model_stop_reason(open, "authority_ended")?;
                return Err(DeviceModelFailure::LeaseExpired);
            }
            before_attempt()?;
            let release = AdmissionRelease(after_attempt);
            if self.model_cancelled(&open.model_exchange_id.0)? {
                self.retain_model_stop_reason(open, "authority_ended")?;
                return Ok(Vec::new());
            }
            if !can_start() {
                self.retain_model_stop_reason(open, "authority_ended")?;
                return Err(DeviceModelFailure::LeaseExpired);
            }
            attempt_number = attempt_number
                .checked_add(1)
                .ok_or(DeviceModelFailure::Unavailable)?;
            let adapter_request_id = format!(
                "device-{}:attempt:{attempt_number}",
                open.model_exchange_id.0
            );
            let adapter = make_adapter(&config, headers.clone(), codex_account.clone())
                .map_err(|_| DeviceModelFailure::InvalidConfiguration)?;
            let Some(cancellation) = self.active_cancellation(&open.model_exchange_id.0)? else {
                return Err(DeviceModelFailure::Unavailable);
            };
            let adapter = adapter
                .with_cancellation(cancellation)
                .with_sse_failure_log(sse_failure_log.clone());
            let inserted = self.connection.execute(
                "INSERT INTO model_invocation_attempts(exchange_id,attempt_number,adapter_request_id,state) SELECT ?1,?2,?3,'prepared' WHERE EXISTS(SELECT 1 FROM exchanges WHERE exchange_id=?1 AND cancelled=0 AND chunks IS NULL) AND NOT EXISTS(SELECT 1 FROM accounting_closed_attempts WHERE job_id=?4 AND attempt=?5)",
                params![open.model_exchange_id.0, attempt_number, adapter_request_id, open.lease.job_id.0, open.lease.attempt],
            )?;
            if inserted != 1 {
                return Err(DeviceModelFailure::Unavailable);
            }
            if self.model_cancelled(&open.model_exchange_id.0)? || !can_start() {
                self.connection.execute("UPDATE model_invocation_attempts SET state='not_sent' WHERE exchange_id=?1 AND attempt_number=?2 AND state='prepared'",params![open.model_exchange_id.0,attempt_number])?;
                if self.model_cancelled(&open.model_exchange_id.0)? {
                    self.retain_model_stop_reason(open, "authority_ended")?;
                    return Ok(Vec::new());
                }
                self.retain_model_stop_reason(open, "authority_ended")?;
                return Err(DeviceModelFailure::LeaseExpired);
            }
            let mut leak_gate = CredentialLeakGate::new();
            leak_gate.track_secret(&secret);
            let invocation = ProviderAdapterInvocation {
                model_exchange_id: &open.model_exchange_id,
                request_id: &open.request_id,
                adapter_request_id: &adapter_request_id,
                model_id: &model_id,
                content_type: &open.request.content_type,
                payload: &payload,
            };
            let diagnostic_sequence = self.begin_model_diagnostic(open, retry.attempt())?;
            self.connection.execute("UPDATE model_invocation_attempts SET state='invoking' WHERE exchange_id=?1 AND attempt_number=?2 AND state='prepared'",params![open.model_exchange_id.0,attempt_number])?;
            let outcome = if let Err(error) = adapter.open(&invocation, &secret) {
                ModelAttemptOutcome::Failed {
                    network: error.network_failure(),
                    chunk: Box::new(model_failure_metadata(
                        open,
                        adapter_error_message(error.kind()),
                        error.retryable(),
                        error.metadata(),
                    )?),
                    retryable: error.retryable(),
                    response: None,
                    accounting: None,
                    not_sent: error.network_failure().acceptance
                        == winwincode_network::Acceptance::NotSent
                        || matches!(
                            error.kind(),
                            crate::ProviderAdapterErrorKind::RequestInvalid
                                | crate::ProviderAdapterErrorKind::RequestTranslation
                                | crate::ProviderAdapterErrorKind::RequestSizeLimit
                                | crate::ProviderAdapterErrorKind::IdentityConflict
                        ),
                }
            } else {
                let receipt = ProviderGatewayOpenReceipt {
                    model_exchange_id: open.model_exchange_id.clone(),
                    request_id: open.request_id.clone(),
                    route: ModelRoute {
                        provider_id: provider_id.clone(),
                        model_id: model_id.clone(),
                        credential_reference_id: CredentialReferenceId(format!(
                            "crd_0{}",
                            &format!("{:X}", Sha256::digest(provider_id.as_bytes()))[..25]
                        )),
                    },
                    adapter_request_id: adapter_request_id.clone(),
                    idempotent_replay: false,
                    stream_leak_gate: leak_gate,
                };
                match adapter.drain_canonical(&receipt) {
                    Ok(completion) => completion_outcome(open, &completion)?,
                    Err(error) => ModelAttemptOutcome::Failed {
                        network: error.network_failure(),
                        chunk: Box::new(model_failure_metadata(
                            open,
                            provider_error_message(error.kind()),
                            error.retryable(),
                            error.metadata(),
                        )?),
                        retryable: error.retryable(),
                        response: error.response().map(<[u8]>::to_vec),
                        accounting: error
                            .observed_receipt()
                            .map(|(response_id, usage)| {
                                observed_accounting_chunk(open, response_id, *usage)
                            })
                            .transpose()?
                            .map(Box::new),
                        not_sent: false,
                    },
                }
            };
            let _ = adapter.control(
                &open.model_exchange_id,
                &adapter_request_id,
                ProviderStreamControlAction::Release,
            );
            let (mut chunks, retryable, network) = match outcome {
                ModelAttemptOutcome::Completed(chunks) => {
                    self.connection.execute("UPDATE model_invocation_attempts SET state='completed',accounting_chunks=?1 WHERE exchange_id=?2 AND attempt_number=?3 AND state='invoking'", params![serde_json::to_string(&chunks)?, open.model_exchange_id.0, attempt_number])?;
                    (chunks, false, None)
                }
                ModelAttemptOutcome::TerminalFailure {
                    chunks,
                    accounting,
                    network,
                } => {
                    self.connection.execute("UPDATE model_invocation_attempts SET state='failed',failure_chunks=?1,accounting_chunks=?2 WHERE exchange_id=?3 AND attempt_number=?4 AND state='invoking'",params![serde_json::to_string(&chunks)?,accounting.map(|chunk|serde_json::to_string(&vec![chunk])).transpose()?,open.model_exchange_id.0,attempt_number])?;
                    (chunks, false, Some(network))
                }
                ModelAttemptOutcome::Failed {
                    chunk,
                    retryable,
                    response,
                    accounting,
                    not_sent,
                    network,
                } => {
                    let mut chunk = *chunk;
                    append_network_failure(&mut chunk, network);
                    let chunks = vec![chunk];
                    self.connection.execute("UPDATE model_invocation_attempts SET state=?6,failure_chunks=?1,accounting_chunks=?2,response_bytes=?3 WHERE exchange_id=?4 AND attempt_number=?5 AND state='invoking'", params![serde_json::to_string(&chunks)?, accounting.map(|chunk| serde_json::to_string(&vec![chunk])).transpose()?, response, open.model_exchange_id.0, attempt_number,if not_sent {"not_sent"} else {"failed"}])?;
                    (chunks, retryable, Some(network))
                }
            };
            self.finish_model_diagnostic(open, diagnostic_sequence, network)?;
            drop(release);
            let delay = network.and_then(|failure| retry.delay_after(&failure));
            if !recover || !retryable || delay.is_none() {
                let reason = if network.is_none() {
                    if chunks.last().is_some_and(|chunk| chunk.error.is_some()) {
                        "permanent_failure"
                    } else {
                        "succeeded"
                    }
                } else if !retryable {
                    "permanent_failure"
                } else if !recover {
                    "recovery_disabled"
                } else {
                    "retry_budget_exhausted"
                };
                self.retain_model_stop_reason(open, reason)?;
                // Runtime replay exposes the finite final failure, while every
                // physical invocation retains its original accounting facts.
                for chunk in &mut chunks {
                    if let Some(error) = &mut chunk.error {
                        error.retryable = false;
                    }
                }
                return Ok(chunks);
            }
            let duration = delay.expect("checked retry delay");
            let started = std::time::Instant::now();
            while started.elapsed() < duration {
                if self.model_cancelled(&open.model_exchange_id.0)? {
                    self.retain_model_stop_reason(open, "authority_ended")?;
                    return Ok(Vec::new());
                }
                if !can_start() {
                    self.retain_model_stop_reason(open, "authority_ended")?;
                    return Err(DeviceModelFailure::LeaseExpired);
                }
                std::thread::sleep(
                    duration
                        .saturating_sub(started.elapsed())
                        .min(std::time::Duration::from_millis(50)),
                );
            }
        }
    }

    fn begin_model_diagnostic(
        &self,
        open: &ModelOpenMessage,
        attempt: u32,
    ) -> Result<i64, DeviceProviderError> {
        let now = i64::try_from(winwincode_network::journal::now_millis())
            .map_err(|_| DeviceProviderError)?;
        self.connection.query_row(
            "INSERT INTO model_attempt_diagnostics(exchange_id,sequence,policy_attempt,started_ms,outcome) SELECT ?1,COALESCE(MAX(sequence),0)+1,?2,?3,'in_flight' FROM model_attempt_diagnostics WHERE exchange_id=?1 RETURNING sequence",
            params![open.model_exchange_id.0,attempt,now], |row| row.get(0)).map_err(Into::into)
    }

    fn finish_model_diagnostic(
        &self,
        open: &ModelOpenMessage,
        sequence: i64,
        failure: Option<winwincode_network::NetworkFailure>,
    ) -> Result<(), DeviceProviderError> {
        let now = i64::try_from(winwincode_network::journal::now_millis())
            .map_err(|_| DeviceProviderError)?;
        let changed = self.connection.execute(
            "UPDATE model_attempt_diagnostics SET finished_ms=?1,outcome=?2,failure_json=?3 WHERE exchange_id=?4 AND sequence=?5 AND outcome='in_flight'",
            params![now,if failure.is_some() {"failed"} else {"accepted"}, failure.map(|value|serde_json::to_string(&value)).transpose()?,open.model_exchange_id.0,sequence])?;
        if changed != 1 {
            return Err(DeviceProviderError);
        }
        Ok(())
    }

    fn retain_model_stop_reason(
        &self,
        open: &ModelOpenMessage,
        reason: &str,
    ) -> Result<(), DeviceProviderError> {
        self.connection.execute("UPDATE model_attempt_diagnostics SET stop_reason=?1 WHERE exchange_id=?2 AND sequence=(SELECT MAX(sequence) FROM model_attempt_diagnostics WHERE exchange_id=?2)",params![reason,open.model_exchange_id.0])?;
        Ok(())
    }

    pub(crate) fn settle_interrupted_invocations(
        &self,
        open: &ModelOpenMessage,
    ) -> Result<(), DeviceProviderError> {
        let chunks = serde_json::to_string(&vec![model_failure(open, "DEVICE_MODEL_INTERRUPTED")])?;
        let failure = serde_json::to_string(&winwincode_network::NetworkFailure::new(
            winwincode_network::ErrorKind::TransportInterrupted,
            winwincode_network::Acceptance::Unknown,
            winwincode_network::Phase::Stream,
        ))?;
        let now = i64::try_from(winwincode_network::journal::now_millis())
            .map_err(|_| DeviceProviderError)?;
        // Accounting can already own a transaction. A savepoint also makes the
        // diagnostic closure atomic with the authoritative invocation closure.
        self.connection
            .execute_batch("SAVEPOINT settle_interrupted_model")?;
        let result = (|| -> Result<(), DeviceProviderError> {
            self.connection.execute("UPDATE model_invocation_attempts SET state=CASE WHEN state='prepared' THEN 'not_sent' ELSE 'interrupted_unknown' END,failure_chunks=CASE WHEN state='invoking' THEN COALESCE(failure_chunks,?1) ELSE failure_chunks END WHERE exchange_id=?2 AND state IN ('prepared','invoking')",params![chunks,open.model_exchange_id.0])?;
            // A process exit does not prove whether a request was accepted.
            // Preserve completed diagnostics and all original paid facts.
            self.connection.execute("UPDATE model_attempt_diagnostics SET finished_ms=?1,outcome='failed',failure_json=?2,stop_reason=COALESCE(stop_reason,'interrupted_unknown') WHERE exchange_id=?3 AND outcome='in_flight'",params![now,failure,open.model_exchange_id.0])?;
            Ok(())
        })();
        let result = result.and_then(|()| {
            self.connection
                .execute_batch("RELEASE settle_interrupted_model")
                .map_err(Into::into)
        });
        if let Err(error) = result {
            let _ = self.connection.execute_batch(
                "ROLLBACK TO settle_interrupted_model; RELEASE settle_interrupted_model",
            );
            return Err(error);
        }
        Ok(())
    }

    pub(crate) fn settle_abandoned_model_accounting(
        &self,
        exchange_id: &str,
    ) -> Result<(), DeviceProviderError> {
        let Some(_owner) = self.try_model_exchange_owner(exchange_id)? else {
            return Ok(());
        };
        let request: Option<Option<String>> = self
            .connection
            .query_row(
                "SELECT request_open FROM exchanges WHERE exchange_id=?1 AND request_open IS NOT NULL",
                [exchange_id],
                |row| row.get(0),
            )
            .optional()?;
        if let Some(Some(request)) = request {
            let open: ModelOpenMessage = serde_json::from_str(&request)?;
            self.settle_interrupted_invocations(&open)?;
            self.connection.execute("UPDATE exchanges SET accounting_chunks=COALESCE(accounting_chunks,chunks,?1) WHERE exchange_id=?2",params![serde_json::to_string(&vec![model_failure(&open,"DEVICE_MODEL_INTERRUPTED")])?,exchange_id])?;
        }
        Ok(())
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
        } else {
            self.settle_abandoned_model_accounting(exchange_id)?;
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
        #[cfg(any(test, feature = "test-support"))]
        crate::audit_model_replay_metrics::record(chunks.len());
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
    let value = chunk
        .payload
        .as_ref()
        .map(|payload| {
            let bytes = STANDARD
                .decode(&payload.data_base64)
                .map_err(|_| DeviceProviderError)?;
            if payload.content_type != "application/json"
                || payload.payload_digest.0 != format!("sha256:{:x}", Sha256::digest(&bytes))
            {
                return Err(DeviceProviderError);
            }
            serde_json::from_slice::<serde_json::Value>(&bytes).map_err(Into::into)
        })
        .transpose()?;
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
    if let Some(error) = &chunk.error {
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
    if chunk.error.is_none()
        && let Some(value) = value
        && value.get("type").and_then(serde_json::Value::as_str) == Some("output_text_delta")
    {
        let delta = value
            .get("delta")
            .and_then(serde_json::Value::as_str)
            .ok_or(DeviceProviderError)?;
        let bytes =
            serde_json::to_vec(&serde_json::json!({"type":"output_text_delta", "delta":delta}))?;
        public.payload = Some(winwincode_execution_port::generated::EncodedPayload {
            content_type: "application/json".to_owned(),
            data_base64: STANDARD.encode(&bytes),
            payload_digest: winwincode_domain::Sha256Digest(format!(
                "sha256:{:x}",
                Sha256::digest(&bytes)
            )),
        });
    }
    Ok(public)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn slow_accounting_scan_does_not_block_a_live_model_invocation() {
        assert_accounting_scan_allows_live_model(false);
    }

    #[test]
    fn slow_accounting_scan_does_not_discard_a_live_provider_response() {
        assert_accounting_scan_allows_live_model(true);
    }

    #[allow(
        clippy::too_many_lines,
        reason = "exercise the accounting scan, live invocation, and exact replay on two real SQLite connections"
    )]
    fn assert_accounting_scan_allows_live_model(after_invocation: bool) {
        use rusqlite::{
            functions::FunctionFlags,
            hooks::{AuthAction, AuthContext, Authorization, TransactionOperation},
            types::Value,
        };
        use std::{
            cell::{Cell, RefCell},
            sync::{
                Arc, Mutex,
                atomic::{AtomicBool, Ordering},
                mpsc,
            },
            time::Duration,
        };
        use winwincode_execution_port::action_enforcement::ActionEnforcementSigningKey;

        let root = std::env::temp_dir().join(format!(
            "wwc-accounting-model-concurrency-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let store = DeviceProviderStore::open(&root).unwrap();
        let accounting = DeviceProviderStore::open(&root).unwrap();
        store
            .connection
            .busy_timeout(Duration::from_millis(50))
            .unwrap();
        let fixture: serde_json::Value = serde_json::from_str(include_str!(
            "../../../tests/fixtures/contracts/execution-port.valid.json"
        ))
        .unwrap();
        let mut old: ModelOpenMessage = serde_json::from_value(
            fixture["messages"]
                .as_array()
                .unwrap()
                .iter()
                .find(|message| message["kind"] == "model.open")
                .unwrap()
                .clone(),
        )
        .unwrap();
        let payload = serde_json::to_vec(
            &serde_json::json!({"provider":"fixture-provider","request":{"model":"fixture-model"}}),
        )
        .unwrap();
        old.request.content_type = "application/json".into();
        old.request.data_base64 = STANDARD.encode(&payload);
        old.request.payload_digest.0 = format!("sha256:{:x}", Sha256::digest(&payload));
        store
            .execute_model_with(&old, &|| true, || {
                Ok(vec![model_failure(&old, "fixture final")])
            })
            .unwrap();

        // Pause the actual accounting SELECT while its SQLite transaction is open.
        // Only this fixture connection replaces JSON extraction; returned values are exact.
        let (scanning, scanned) = mpsc::channel();
        let (release, released) = mpsc::channel();
        let released = Mutex::new(released);
        let paused = AtomicBool::new(false);
        // Recovery probes run before the financial snapshot. Observe BEGIN on
        // this connection so the fixture pauses the subsequent transactional
        // receipt scan, preserving the original stale-WAL-snapshot regression.
        let transaction_started = Arc::new(AtomicBool::new(false));
        let observed = Arc::clone(&transaction_started);
        accounting
            .connection
            .authorizer(Some(move |context: AuthContext<'_>| {
                if matches!(
                    context.action,
                    AuthAction::Transaction {
                        operation: TransactionOperation::Begin
                    }
                ) {
                    observed.store(true, Ordering::SeqCst);
                }
                Authorization::Allow
            }))
            .unwrap();
        accounting
            .connection
            .create_scalar_function(
                "json_extract",
                2,
                FunctionFlags::SQLITE_UTF8,
                move |context| {
                    if transaction_started.load(Ordering::SeqCst)
                        && !paused.swap(true, Ordering::SeqCst)
                    {
                        scanning.send(()).unwrap();
                        released
                            .lock()
                            .unwrap()
                            .recv_timeout(Duration::from_secs(10))
                            .unwrap();
                    }
                    let json: serde_json::Value =
                        serde_json::from_str(&context.get::<String>(0)?).unwrap();
                    Ok(match context.get::<String>(1)?.as_str() {
                        "$.lease.jobId" => {
                            Value::Text(json["lease"]["jobId"].as_str().unwrap().to_owned())
                        }
                        "$.lease.attempt" => {
                            Value::Integer(json["lease"]["attempt"].as_i64().unwrap())
                        }
                        path => panic!("unexpected accounting JSON path: {path}"),
                    })
                },
            )
            .unwrap();
        let accounting = RefCell::new(Some(accounting));
        let task = RefCell::new(None);
        let start_scan = || {
            let accounting = accounting.borrow_mut().take().unwrap();
            let lease = old.lease.clone();
            *task.borrow_mut() = Some(std::thread::spawn(move || {
                accounting.accounting_statement(
                    &lease,
                    &ActionEnforcementSigningKey::from_bytes([7; 32]).unwrap(),
                )
            }));
            scanned.recv_timeout(Duration::from_secs(10)).unwrap();
        };
        if !after_invocation {
            start_scan();
        }
        let mut live = old.clone();
        live.model_exchange_id.0 = "mdl_00000000000000000000000999".into();
        live.lease.job_id.0 = "job_00000000000000000000000999".into();
        live.lease.lease_id.0 = "lse_00000000000000000000000999".into();
        let calls = Cell::new(0);
        let invoke = || {
            calls.set(calls.get() + 1);
            if after_invocation {
                start_scan();
            }
            let mut chunk = model_chunk(&live, 1);
            chunk.is_final = true;
            Ok(vec![chunk])
        };
        let result = store.execute_model_with(&live, &|| true, invoke);
        release.send(()).unwrap();
        let scan_result = task.borrow_mut().take().unwrap().join().unwrap();
        assert!(
            result.is_ok(),
            "auxiliary accounting changed a live model outcome (after_invocation={after_invocation}): {result:?}"
        );
        assert_eq!(calls.get(), 1);
        assert_eq!(
            store.execute_model_with(&live, &|| false, invoke).unwrap(),
            result.unwrap()
        );
        assert_eq!(calls.get(), 1, "recovery cannot invoke the Provider again");
        assert!(
            scan_result.is_err(),
            "a stale accounting snapshot must retry rather than close a changed database"
        );
        let key = ActionEnforcementSigningKey::from_bytes([7; 32]).unwrap();
        let statement = store
            .accounting_statement(&old.lease, &key)
            .unwrap()
            .unwrap();
        statement.verify(&key).unwrap();
        let mut forbidden = old.clone();
        forbidden.model_exchange_id.0 = "mdl_00000000000000000000000998".into();
        assert!(
            store
                .execute_model_with(&forbidden, &|| true, invoke)
                .is_err()
        );
        assert_eq!(calls.get(), 1, "the closed attempt remains fenced");
        drop(store);
        std::fs::remove_dir_all(root).unwrap();
    }

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

fn append_network_failure(
    chunk: &mut ModelChunkMessage,
    failure: winwincode_network::NetworkFailure,
) {
    if let Some(error) = chunk.error.as_mut() {
        // Complete facts are already durable in model_attempt_diagnostics.
        // The bounded wire message carries a compact view, never partial JSON.
        let mut wire = failure;
        let Ok(mut facts) = serde_json::to_string(&wire) else {
            return;
        };
        if facts.chars().count() > 430 {
            if let Some(diagnostic) = wire.diagnostic.as_mut() {
                diagnostic.line = None;
                diagnostic.column = None;
                diagnostic.os_code = None;
                diagnostic.io_kind = None;
            }
            let Ok(compact) = serde_json::to_string(&wire) else {
                return;
            };
            facts = compact;
        }
        let suffix = format!(";network={facts}");
        let available = 500_usize.saturating_sub(suffix.chars().count());
        error.message = error.message.chars().take(available).collect();
        error.message.push_str(&suffix);
    }
}

#[cfg(test)]
#[path = "device_model_diagnostics_regression_tests.rs"]
mod diagnostics_regression_tests;
