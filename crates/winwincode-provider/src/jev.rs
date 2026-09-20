// SPDX-License-Identifier: Apache-2.0

//! Provider-neutral hypothesis scoring used by the context runtime.
//!
//! `OpenJev` adapters sit behind [`JevProvider`] only. Runtime callers never see
//! `OpenJev` or `TypeSafe` types, and Jev outage fail-opens so Coding Agent Worker
//! continues without a decision.

use std::{
    fmt,
    io::Read as _,
    sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    },
    time::Duration,
};

use futures::future::BoxFuture;
use serde::{Deserialize, Serialize};
use tokio::time::{Instant, timeout};

use crate::canonical_https_endpoint;

/// Documented local `OpenJev` runtime gap for JEV-01.
///
/// `WinWinCode` does not embed `OpenJev` weights, CUDA/MPS kernels, or a second
/// model wire protocol. Local scoring is available only when a host injects
/// [`OpenJevLocalRuntime`]. Without that injection the local provider stays on
/// the portable contract, reports [`JevHealth::Unavailable`], and
/// [`JevRuntime`] fail-opens to Remote/Mock providers or skips Jev.
pub const OPENJEV_LOCAL_RUNTIME_GAP: &str = "OpenJev local model runtime is not embedded in winwincode-provider; inject OpenJevLocalRuntime when a pinned model/runtime is available.";

/// Preferred `OpenJev` model identity used when hosts do not override it.
pub const OPENJEV_DEFAULT_MODEL_ID: &str = "openjev-4b-v2";

/// One natural-language inference request.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct JevHypothesis {
    pub premise: String,
    pub hypothesis: String,
}

/// Provider-neutral NLI probabilities.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct JevScores {
    pub entailment: f32,
    pub contradiction: f32,
    pub neutral: f32,
}

impl JevScores {
    /// Builds normalized finite probabilities.
    ///
    /// # Errors
    ///
    /// Rejects values outside `[0, 1]` or totals outside a small rounding tolerance.
    pub fn try_new(
        entailment: f32,
        contradiction: f32,
        neutral: f32,
    ) -> Result<Self, JevProviderError> {
        let scores = Self {
            entailment,
            contradiction,
            neutral,
        };
        if !scores.valid() {
            return Err(JevProviderError::invalid_response());
        }
        Ok(scores)
    }

    #[must_use]
    pub fn confidence(self) -> f32 {
        self.entailment.max(self.contradiction).max(self.neutral)
    }

    fn valid(self) -> bool {
        let values = [self.entailment, self.contradiction, self.neutral];
        values
            .iter()
            .all(|value| value.is_finite() && (0.0..=1.0).contains(value))
            && (values.iter().sum::<f32>() - 1.0).abs() <= 0.01
    }
}

/// Portable inference device selection.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum JevDevice {
    Auto,
    Cuda,
    Mps,
    Cpu,
}

/// Portable inference numeric representation.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum JevDtype {
    Auto,
    Float32,
    Float16,
    Bfloat16,
    Int8,
    Int4,
}

/// Settings passed unchanged to each replaceable Provider.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct JevExecutionOptions {
    pub device: JevDevice,
    pub dtype: JevDtype,
}

/// One scored hypothesis and its Provider-reported accounting.
#[derive(Clone, Debug, PartialEq)]
pub struct JevEvaluation {
    pub scores: JevScores,
    pub input_tokens: u64,
    /// Resolved device. Providers must not return [`JevDevice::Auto`].
    pub device: JevDevice,
}

/// One batch result and its Provider-reported accounting.
#[derive(Clone, Debug, PartialEq)]
pub struct JevBatchEvaluation {
    pub evaluations: Vec<JevScores>,
    pub input_tokens: u64,
    /// Resolved device. Providers must not return [`JevDevice::Auto`].
    pub device: JevDevice,
}

/// Stable Provider health state.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum JevHealth {
    Healthy,
    Degraded,
    Unavailable,
}

/// Provider-neutral discovery facts used before an inference call.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct JevProviderCapabilities {
    pub provider_id: String,
    pub model_id: String,
    pub max_batch_size: usize,
    pub devices: Vec<JevDevice>,
    pub dtypes: Vec<JevDtype>,
}

impl JevProviderCapabilities {
    fn supports(&self, batch_size: usize, options: JevExecutionOptions) -> bool {
        batch_size > 0
            && batch_size <= self.max_batch_size
            && (options.device == JevDevice::Auto || self.devices.contains(&options.device))
            && (options.dtype == JevDtype::Auto || self.dtypes.contains(&options.dtype))
    }
}

/// Stable failure categories safe for fallback policy and telemetry.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum JevProviderErrorKind {
    InvalidRequest,
    Unsupported,
    Unavailable,
    ResourceExhausted,
    Timeout,
    InvalidResponse,
}

/// Provider error that never carries an upstream response body.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct JevProviderError {
    kind: JevProviderErrorKind,
    message: &'static str,
}

impl JevProviderError {
    #[must_use]
    pub const fn new(kind: JevProviderErrorKind) -> Self {
        let message = match kind {
            JevProviderErrorKind::InvalidRequest => "Jev Provider request is invalid",
            JevProviderErrorKind::Unsupported => "Jev Provider capability is unsupported",
            JevProviderErrorKind::Unavailable => "Jev Provider is unavailable",
            JevProviderErrorKind::ResourceExhausted => "Jev Provider resource is exhausted",
            JevProviderErrorKind::Timeout => "Jev Provider timed out",
            JevProviderErrorKind::InvalidResponse => "Jev Provider response is invalid",
        };
        Self { kind, message }
    }

    const fn invalid_response() -> Self {
        Self::new(JevProviderErrorKind::InvalidResponse)
    }

    #[must_use]
    pub const fn kind(&self) -> JevProviderErrorKind {
        self.kind
    }

    const fn retryable(&self) -> bool {
        matches!(
            self.kind,
            JevProviderErrorKind::Unavailable
                | JevProviderErrorKind::ResourceExhausted
                | JevProviderErrorKind::Timeout
        )
    }
}

impl fmt::Display for JevProviderError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.message)
    }
}

impl std::error::Error for JevProviderError {}

/// Replaceable hypothesis-scoring Provider boundary.
///
/// Like the Kernel's `ModelPort`, calls return owned futures and expose only
/// portable host types. Local `OpenJev`, remote services, and tests implement the
/// same boundary without adding another model wire protocol.
pub trait JevProvider: fmt::Debug + Send + Sync {
    fn evaluate(
        &self,
        input: JevHypothesis,
        options: JevExecutionOptions,
    ) -> BoxFuture<'static, Result<JevEvaluation, JevProviderError>>;

    fn batch_evaluate(
        &self,
        inputs: Vec<JevHypothesis>,
        options: JevExecutionOptions,
    ) -> BoxFuture<'static, Result<JevBatchEvaluation, JevProviderError>>;

    fn health(&self) -> BoxFuture<'static, JevHealth>;

    fn capabilities(&self) -> JevProviderCapabilities;
}

/// Retry and timeout policy shared by ordered Local/Remote Provider choices.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct JevRuntimeConfig {
    pub timeout: Duration,
    /// Retries after the initial call. Only transient failures are retried.
    pub retries: u8,
}

/// Safe facts emitted after a successful inference.
#[derive(Clone, Debug, PartialEq)]
pub struct JevObservation {
    pub provider_id: String,
    pub model_id: String,
    pub latency: Duration,
    pub input_tokens: u64,
    pub batch_size: usize,
    pub device: JevDevice,
    pub confidence: f32,
}

/// One failed attempt retained without Provider payloads.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct JevAttemptFailure {
    pub provider_id: String,
    pub kind: JevProviderErrorKind,
    pub latency: Duration,
}

/// Fail-open inference result. `value == None` means the caller continues
/// without a Jev decision.
#[derive(Clone, Debug, PartialEq)]
pub struct JevRun<T> {
    pub value: Option<T>,
    pub observation: Option<JevObservation>,
    pub failures: Vec<JevAttemptFailure>,
}

trait MeasuredJevResult {
    fn observation(
        &self,
        capabilities: &JevProviderCapabilities,
        latency: Duration,
        expected_batch_size: usize,
    ) -> Option<JevObservation>;
}

impl MeasuredJevResult for JevEvaluation {
    fn observation(
        &self,
        capabilities: &JevProviderCapabilities,
        latency: Duration,
        expected_batch_size: usize,
    ) -> Option<JevObservation> {
        (expected_batch_size == 1 && self.device != JevDevice::Auto && self.scores.valid()).then(
            || JevObservation {
                provider_id: capabilities.provider_id.clone(),
                model_id: capabilities.model_id.clone(),
                latency,
                input_tokens: self.input_tokens,
                batch_size: 1,
                device: self.device,
                confidence: self.scores.confidence(),
            },
        )
    }
}

impl MeasuredJevResult for JevBatchEvaluation {
    fn observation(
        &self,
        capabilities: &JevProviderCapabilities,
        latency: Duration,
        expected_batch_size: usize,
    ) -> Option<JevObservation> {
        (self.evaluations.len() == expected_batch_size
            && self.device != JevDevice::Auto
            && self.evaluations.iter().all(|scores| scores.valid()))
        .then(|| JevObservation {
            provider_id: capabilities.provider_id.clone(),
            model_id: capabilities.model_id.clone(),
            latency,
            input_tokens: self.input_tokens,
            batch_size: self.evaluations.len(),
            device: self.device,
            confidence: self
                .evaluations
                .iter()
                .map(|scores| scores.confidence())
                .fold(1.0, f32::min),
        })
    }
}

/// Ordered, dependency-injected Provider runner with retry, timeout, fallback,
/// and final skip behavior.
#[derive(Debug)]
pub struct JevRuntime {
    providers: Vec<Arc<dyn JevProvider>>,
    config: JevRuntimeConfig,
}

impl JevRuntime {
    #[must_use]
    pub fn new(providers: Vec<Arc<dyn JevProvider>>, config: JevRuntimeConfig) -> Self {
        Self { providers, config }
    }

    pub async fn evaluate(
        &self,
        input: JevHypothesis,
        options: JevExecutionOptions,
    ) -> JevRun<JevEvaluation> {
        self.run(1, options, move |provider| {
            provider.evaluate(input.clone(), options)
        })
        .await
    }

    pub async fn batch_evaluate(
        &self,
        inputs: Vec<JevHypothesis>,
        options: JevExecutionOptions,
    ) -> JevRun<JevBatchEvaluation> {
        let batch_size = inputs.len();
        self.run(batch_size, options, move |provider| {
            provider.batch_evaluate(inputs.clone(), options)
        })
        .await
    }

    async fn run<T>(
        &self,
        batch_size: usize,
        options: JevExecutionOptions,
        operation: impl Fn(Arc<dyn JevProvider>) -> BoxFuture<'static, Result<T, JevProviderError>>,
    ) -> JevRun<T>
    where
        T: MeasuredJevResult,
    {
        let mut failures = Vec::new();
        for provider in &self.providers {
            let capabilities = provider.capabilities();
            if !capabilities.supports(batch_size, options) {
                failures.push(JevAttemptFailure {
                    provider_id: capabilities.provider_id,
                    kind: JevProviderErrorKind::Unsupported,
                    latency: Duration::ZERO,
                });
                continue;
            }
            for attempt in 0..=self.config.retries {
                let started = Instant::now();
                let result = timeout(self.config.timeout, operation(Arc::clone(provider))).await;
                let latency = started.elapsed();
                match result {
                    Ok(Ok(value)) => {
                        if let Some(observation) =
                            value.observation(&capabilities, latency, batch_size)
                        {
                            return JevRun {
                                value: Some(value),
                                observation: Some(observation),
                                failures,
                            };
                        }
                        failures.push(JevAttemptFailure {
                            provider_id: capabilities.provider_id.clone(),
                            kind: JevProviderErrorKind::InvalidResponse,
                            latency,
                        });
                        break;
                    }
                    Ok(Err(error)) => {
                        let retryable = error.retryable();
                        failures.push(JevAttemptFailure {
                            provider_id: capabilities.provider_id.clone(),
                            kind: error.kind(),
                            latency,
                        });
                        if !retryable || attempt == self.config.retries {
                            break;
                        }
                    }
                    Err(_) => failures.push(JevAttemptFailure {
                        provider_id: capabilities.provider_id.clone(),
                        kind: JevProviderErrorKind::Timeout,
                        latency,
                    }),
                }
            }
        }
        JevRun {
            value: None,
            observation: None,
            failures,
        }
    }
}

/// Fallback ordering policy for Local/Remote `OpenJev` providers.
///
/// The runtime itself always fail-opens: exhausted providers skip Jev rather
/// than aborting Coding Agent Worker.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum JevFallbackPolicy {
    /// Only the Local provider is attempted.
    LocalOnly,
    /// Only the Remote provider is attempted.
    RemoteOnly,
    /// Local first, then Remote.
    LocalFirst,
    /// Remote first, then Local.
    RemoteFirst,
    /// Explicit fallback chain (currently Local then Remote).
    Fallback,
    /// No providers; every call skips Jev.
    Skip,
}

/// Builds the ordered provider list for one policy.
#[must_use]
pub fn jev_provider_order(
    policy: JevFallbackPolicy,
    local: Option<Arc<dyn JevProvider>>,
    remote: Option<Arc<dyn JevProvider>>,
) -> Vec<Arc<dyn JevProvider>> {
    match policy {
        JevFallbackPolicy::Skip => Vec::new(),
        JevFallbackPolicy::LocalOnly => local.into_iter().collect(),
        JevFallbackPolicy::RemoteOnly => remote.into_iter().collect(),
        JevFallbackPolicy::LocalFirst | JevFallbackPolicy::Fallback => {
            local.into_iter().chain(remote).collect()
        }
        JevFallbackPolicy::RemoteFirst => remote.into_iter().chain(local).collect(),
    }
}

/// Injectable local `OpenJev` runtime used when a pinned model is available.
///
/// The host owns weights, device kernels, and batching. This boundary stays
/// provider-neutral so Runtime code never imports `OpenJev` types.
pub trait OpenJevLocalRuntime: fmt::Debug + Send + Sync {
    fn score(
        &self,
        inputs: Vec<JevHypothesis>,
        options: JevExecutionOptions,
    ) -> BoxFuture<'static, Result<JevBatchEvaluation, JevProviderError>>;

    fn health(&self) -> JevHealth;

    fn capabilities(&self) -> JevProviderCapabilities;
}

/// Local `OpenJev` adapter. Without an injected runtime it reports the
/// documented gap instead of inventing scores.
#[derive(Clone, Debug)]
pub struct OpenJevLocalProvider {
    runtime: Option<Arc<dyn OpenJevLocalRuntime>>,
    fallback_capabilities: JevProviderCapabilities,
}

impl OpenJevLocalProvider {
    /// Builds the documented-gap local provider.
    #[must_use]
    pub fn with_runtime_gap() -> Self {
        Self::with_runtime_gap_model(OPENJEV_DEFAULT_MODEL_ID)
    }

    /// Builds a gap provider that still advertises expected `OpenJev` capabilities.
    #[must_use]
    pub fn with_runtime_gap_model(model_id: impl Into<String>) -> Self {
        Self {
            runtime: None,
            fallback_capabilities: openjev_capabilities("openjev-local", model_id.into()),
        }
    }

    /// Builds a local provider backed by an injected `OpenJev` runtime.
    #[must_use]
    pub fn with_runtime(runtime: Arc<dyn OpenJevLocalRuntime>) -> Self {
        let fallback_capabilities = runtime.capabilities();
        Self {
            runtime: Some(runtime),
            fallback_capabilities,
        }
    }

    /// Returns true when a local runtime is present.
    #[must_use]
    pub const fn has_runtime(&self) -> bool {
        self.runtime.is_some()
    }

    fn unavailable() -> JevProviderError {
        JevProviderError::new(JevProviderErrorKind::Unavailable)
    }
}

impl JevProvider for OpenJevLocalProvider {
    fn evaluate(
        &self,
        input: JevHypothesis,
        options: JevExecutionOptions,
    ) -> BoxFuture<'static, Result<JevEvaluation, JevProviderError>> {
        let runtime = self.runtime.clone();
        Box::pin(async move {
            let runtime = runtime.ok_or_else(Self::unavailable)?;
            let batch = runtime.score(vec![input], options).await?;
            if batch.evaluations.len() != 1 {
                return Err(JevProviderError::invalid_response());
            }
            Ok(JevEvaluation {
                scores: batch.evaluations[0],
                input_tokens: batch.input_tokens,
                device: resolve_device(options.device, batch.device),
            })
        })
    }

    fn batch_evaluate(
        &self,
        inputs: Vec<JevHypothesis>,
        options: JevExecutionOptions,
    ) -> BoxFuture<'static, Result<JevBatchEvaluation, JevProviderError>> {
        let runtime = self.runtime.clone();
        Box::pin(async move {
            let runtime = runtime.ok_or_else(Self::unavailable)?;
            let mut batch = runtime.score(inputs, options).await?;
            batch.device = resolve_device(options.device, batch.device);
            Ok(batch)
        })
    }

    fn health(&self) -> BoxFuture<'static, JevHealth> {
        let health = match &self.runtime {
            Some(runtime) => runtime.health(),
            None => JevHealth::Unavailable,
        };
        Box::pin(async move { health })
    }

    fn capabilities(&self) -> JevProviderCapabilities {
        match &self.runtime {
            Some(runtime) => runtime.capabilities(),
            None => self.fallback_capabilities.clone(),
        }
    }
}

/// Construction input for [`OpenJevRemoteConfig::try_new`].
#[derive(Clone, Debug)]
pub struct OpenJevRemoteConfigRequest {
    pub provider_id: String,
    pub endpoint: String,
    pub model_id: String,
    pub max_batch_size: usize,
    pub devices: Vec<JevDevice>,
    pub dtypes: Vec<JevDtype>,
    pub timeout: Duration,
    pub api_key: Option<String>,
}

/// Bounded remote OpenJev/NLI endpoint configuration.
///
/// Secrets are never printed. Endpoint validation reuses the Device Provider
/// HTTPS rule so Jev Remote cannot invent a second transport protocol.
#[derive(Clone)]
pub struct OpenJevRemoteConfig {
    provider_id: String,
    endpoint: String,
    model_id: String,
    max_batch_size: usize,
    devices: Vec<JevDevice>,
    dtypes: Vec<JevDtype>,
    timeout: Duration,
    api_key: Option<String>,
}

impl OpenJevRemoteConfig {
    /// Validates one remote NLI endpoint configuration.
    ///
    /// # Errors
    ///
    /// Rejects non-HTTPS endpoints, empty identities, empty capability lists,
    /// non-positive batch sizes, and unsafe timeouts.
    pub fn try_new(request: OpenJevRemoteConfigRequest) -> Result<Self, JevProviderError> {
        let OpenJevRemoteConfigRequest {
            provider_id,
            endpoint,
            model_id,
            max_batch_size,
            devices,
            dtypes,
            timeout,
            api_key,
        } = request;
        if !valid_identity(&provider_id)
            || !valid_identity(&model_id)
            || !canonical_https_endpoint(&endpoint)
            || max_batch_size == 0
            || max_batch_size > 1_024
            || devices.is_empty()
            || dtypes.is_empty()
            || timeout.is_zero()
            || timeout > Duration::from_mins(2)
        {
            return Err(JevProviderError::new(JevProviderErrorKind::InvalidRequest));
        }
        if let Some(api_key) = &api_key
            && (api_key.is_empty()
                || api_key.len() > 16 * 1024
                || api_key.trim() != api_key
                || api_key.chars().any(char::is_control))
        {
            return Err(JevProviderError::new(JevProviderErrorKind::InvalidRequest));
        }
        Ok(Self {
            provider_id,
            endpoint,
            model_id,
            max_batch_size,
            devices,
            dtypes,
            timeout,
            api_key,
        })
    }

    #[must_use]
    pub fn endpoint(&self) -> &str {
        &self.endpoint
    }

    #[must_use]
    pub const fn timeout(&self) -> Duration {
        self.timeout
    }

    #[must_use]
    pub fn provider_id(&self) -> &str {
        &self.provider_id
    }

    #[must_use]
    pub fn model_id(&self) -> &str {
        &self.model_id
    }

    fn capabilities(&self) -> JevProviderCapabilities {
        JevProviderCapabilities {
            provider_id: self.provider_id.clone(),
            model_id: self.model_id.clone(),
            max_batch_size: self.max_batch_size,
            devices: self.devices.clone(),
            dtypes: self.dtypes.clone(),
        }
    }
}

impl fmt::Debug for OpenJevRemoteConfig {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("OpenJevRemoteConfig")
            .field("provider_id", &self.provider_id)
            .field("endpoint", &self.endpoint)
            .field("model_id", &self.model_id)
            .field("max_batch_size", &self.max_batch_size)
            .field("devices", &self.devices)
            .field("dtypes", &self.dtypes)
            .field("timeout", &self.timeout)
            .field("api_key", &self.api_key.as_ref().map(|_| "[REDACTED]"))
            .finish()
    }
}

/// Remote settings shape shared by YAML/TOML/JSON host configuration.
///
/// Field names match the JEV design notes: `provider`/`endpoint`/`api_key`/
/// `timeout`/`retries`. Hosts deserialize this struct from their config file.
#[derive(Clone, Debug, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct OpenJevRemoteSettings {
    pub provider_id: String,
    pub endpoint: String,
    pub api_key: Option<String>,
    pub model_id: Option<String>,
    pub timeout_ms: u64,
    pub retries: u8,
    pub max_batch_size: Option<usize>,
    pub devices: Option<Vec<String>>,
    pub dtypes: Option<Vec<String>>,
}

impl OpenJevRemoteSettings {
    /// Parses settings from TOML (workspace already pins `toml`).
    ///
    /// # Errors
    ///
    /// Rejects malformed TOML and fields that fail [`OpenJevRemoteConfig::try_new`].
    pub fn from_toml(input: &str) -> Result<Self, JevProviderError> {
        toml::from_str(input).map_err(|_| JevProviderError::new(JevProviderErrorKind::InvalidRequest))
    }

    /// Converts host settings into a validated remote Provider config plus runtime retry policy.
    ///
    /// # Errors
    ///
    /// Rejects invalid identities, endpoints, devices, dtypes, or timeouts.
    pub fn to_config_and_runtime(
        &self,
    ) -> Result<(OpenJevRemoteConfig, JevRuntimeConfig), JevProviderError> {
        let devices = match &self.devices {
            Some(values) if !values.is_empty() => values
                .iter()
                .map(|value| parse_device(value))
                .collect::<Option<Vec<_>>>()
                .ok_or_else(|| JevProviderError::new(JevProviderErrorKind::InvalidRequest))?,
            Some(_) => return Err(JevProviderError::new(JevProviderErrorKind::InvalidRequest)),
            None => vec![JevDevice::Cpu, JevDevice::Cuda, JevDevice::Mps],
        };
        let dtypes = match &self.dtypes {
            Some(values) if !values.is_empty() => values
                .iter()
                .map(|value| parse_dtype(value))
                .collect::<Option<Vec<_>>>()
                .ok_or_else(|| JevProviderError::new(JevProviderErrorKind::InvalidRequest))?,
            Some(_) => return Err(JevProviderError::new(JevProviderErrorKind::InvalidRequest)),
            None => vec![
                JevDtype::Float32,
                JevDtype::Float16,
                JevDtype::Bfloat16,
                JevDtype::Int8,
                JevDtype::Int4,
            ],
        };
        let config = OpenJevRemoteConfig::try_new(OpenJevRemoteConfigRequest {
            provider_id: self.provider_id.clone(),
            endpoint: self.endpoint.clone(),
            model_id: self
                .model_id
                .clone()
                .unwrap_or_else(|| OPENJEV_DEFAULT_MODEL_ID.to_owned()),
            max_batch_size: self.max_batch_size.unwrap_or(8),
            devices,
            dtypes,
            timeout: Duration::from_millis(self.timeout_ms),
            api_key: self.api_key.clone(),
        })?;
        let runtime = JevRuntimeConfig {
            timeout: config.timeout(),
            retries: self.retries,
        };
        Ok((config, runtime))
    }
}

/// Transport used by the remote `OpenJev` adapter. Production uses HTTPS;
/// tests inject portable fakes without changing Provider-neutral types.
pub trait JevRemoteTransport: fmt::Debug + Send + Sync {
    fn score(
        &self,
        model_id: String,
        items: Vec<JevHypothesis>,
        options: JevExecutionOptions,
    ) -> BoxFuture<'static, Result<RemoteJevScoreBatch, JevProviderError>>;
}

/// Provider-neutral remote scoring payload.
#[derive(Clone, Debug, PartialEq)]
pub struct RemoteJevScoreBatch {
    pub evaluations: Vec<JevScores>,
    pub input_tokens: u64,
    pub device: JevDevice,
}

/// Remote OpenJev/NLI adapter over an injected transport.
#[derive(Clone, Debug)]
pub struct OpenJevRemoteProvider {
    config: OpenJevRemoteConfig,
    transport: Arc<dyn JevRemoteTransport>,
}

impl OpenJevRemoteProvider {
    #[must_use]
    pub fn new(config: OpenJevRemoteConfig, transport: Arc<dyn JevRemoteTransport>) -> Self {
        Self { config, transport }
    }
}

impl JevProvider for OpenJevRemoteProvider {
    fn evaluate(
        &self,
        input: JevHypothesis,
        options: JevExecutionOptions,
    ) -> BoxFuture<'static, Result<JevEvaluation, JevProviderError>> {
        let config = self.config.clone();
        let transport = Arc::clone(&self.transport);
        Box::pin(async move {
            let batch = transport
                .score(config.model_id.clone(), vec![input], options)
                .await?;
            if batch.evaluations.len() != 1 {
                return Err(JevProviderError::invalid_response());
            }
            Ok(JevEvaluation {
                scores: batch.evaluations[0],
                input_tokens: batch.input_tokens,
                device: resolve_device(options.device, batch.device),
            })
        })
    }

    fn batch_evaluate(
        &self,
        inputs: Vec<JevHypothesis>,
        options: JevExecutionOptions,
    ) -> BoxFuture<'static, Result<JevBatchEvaluation, JevProviderError>> {
        let config = self.config.clone();
        let transport = Arc::clone(&self.transport);
        Box::pin(async move {
            let batch = transport
                .score(config.model_id.clone(), inputs, options)
                .await?;
            Ok(JevBatchEvaluation {
                evaluations: batch.evaluations,
                input_tokens: batch.input_tokens,
                device: resolve_device(options.device, batch.device),
            })
        })
    }

    fn health(&self) -> BoxFuture<'static, JevHealth> {
        Box::pin(async move { JevHealth::Healthy })
    }

    fn capabilities(&self) -> JevProviderCapabilities {
        self.config.capabilities()
    }
}

/// HTTPS transport for remote NLI scoring.
///
/// Request/response stay provider-neutral JSON. The adapter does not force one
/// `OpenJev` output schema: labeled NLI probabilities and direct score objects
/// are both accepted.
#[derive(Clone, Debug)]
pub struct HttpsJevRemoteTransport {
    agent: ureq::Agent,
    endpoint: String,
    api_key: Option<String>,
}

impl HttpsJevRemoteTransport {
    /// Builds a verified HTTPS transport from one remote configuration.
    ///
    /// # Errors
    ///
    /// Rejects non-HTTPS or otherwise invalid endpoints.
    pub fn try_new(config: &OpenJevRemoteConfig) -> Result<Self, JevProviderError> {
        if !canonical_https_endpoint(config.endpoint()) {
            return Err(JevProviderError::new(JevProviderErrorKind::InvalidRequest));
        }
        let agent = ureq::Agent::config_builder()
            .http_status_as_error(false)
            .max_redirects(0)
            .proxy(None)
            .timeout_connect(Some(config.timeout()))
            .timeout_recv_body(Some(config.timeout()))
            .timeout_global(Some(config.timeout()))
            .tls_config(
                ureq::tls::TlsConfig::builder()
                    .provider(ureq::tls::TlsProvider::Rustls)
                    .root_certs(ureq::tls::RootCerts::WebPki)
                    .use_sni(true)
                    .disable_verification(false)
                    .build(),
            )
            .build()
            .into();
        Ok(Self {
            agent,
            endpoint: config.endpoint().to_owned(),
            api_key: config.api_key.clone(),
        })
    }

    #[allow(
        clippy::needless_pass_by_value,
        reason = "trait signature is owned for 'static BoxFuture; values are moved into spawn_blocking"
    )]
    fn score_blocking(
        &self,
        model_id: String,
        items: Vec<JevHypothesis>,
        options: JevExecutionOptions,
    ) -> Result<RemoteJevScoreBatch, JevProviderError> {
        let body = serde_json::json!({
            "model": model_id,
            "device": device_name(options.device),
            "dtype": dtype_name(options.dtype),
            "items": items.iter().map(|item| serde_json::json!({
                "premise": item.premise,
                "hypothesis": item.hypothesis,
            })).collect::<Vec<_>>(),
        });
        let mut request = self
            .agent
            .post(&self.endpoint)
            .header("content-type", "application/json")
            .header("accept", "application/json");
        if let Some(api_key) = &self.api_key {
            request = request.header("authorization", format!("Bearer {api_key}"));
        }
        let body = serde_json::to_vec(&body)
            .map_err(|_| JevProviderError::new(JevProviderErrorKind::InvalidRequest))?;
        let response = request
            .send(body.as_slice())
            .map_err(|_| JevProviderError::new(JevProviderErrorKind::Unavailable))?;
        let status = response.status();
        if !(200..300).contains(&status.as_u16()) {
            return Err(JevProviderError::new(match status.as_u16() {
                408 | 425 | 429 => JevProviderErrorKind::ResourceExhausted,
                400..=499 => JevProviderErrorKind::InvalidRequest,
                _ => JevProviderErrorKind::Unavailable,
            }));
        }
        let mut bytes = Vec::new();
        response
            .into_body()
            .as_reader()
            .read_to_end(&mut bytes)
            .map_err(|_| JevProviderError::new(JevProviderErrorKind::Unavailable))?;
        if bytes.len() > 2 * 1024 * 1024 {
            return Err(JevProviderError::invalid_response());
        }
        let value: serde_json::Value =
            serde_json::from_slice(&bytes).map_err(|_| JevProviderError::invalid_response())?;
        let evaluations = parse_jev_score_list(&value)?;
        let input_tokens = value
            .get("input_tokens")
            .and_then(serde_json::Value::as_u64)
            .unwrap_or_else(|| estimate_tokens(&items));
        let device = value
            .get("device")
            .and_then(serde_json::Value::as_str)
            .and_then(parse_device)
            .unwrap_or(resolve_device(options.device, JevDevice::Cpu));
        Ok(RemoteJevScoreBatch {
            evaluations,
            input_tokens,
            device,
        })
    }
}

impl JevRemoteTransport for HttpsJevRemoteTransport {
    fn score(
        &self,
        model_id: String,
        items: Vec<JevHypothesis>,
        options: JevExecutionOptions,
    ) -> BoxFuture<'static, Result<RemoteJevScoreBatch, JevProviderError>> {
        let this = self.clone();
        Box::pin(async move {
            tokio::task::spawn_blocking(move || this.score_blocking(model_id, items, options))
                .await
                .map_err(|_| JevProviderError::new(JevProviderErrorKind::Unavailable))?
        })
    }
}

/// Scripted remote transport for tests and local gates that need
/// [`OpenJevRemoteProvider`] without a live endpoint.
#[derive(Clone, Debug)]
pub struct MockJevRemoteTransport {
    evaluations: Vec<JevScores>,
    input_tokens: u64,
    device: JevDevice,
    error: Option<JevProviderErrorKind>,
}

impl MockJevRemoteTransport {
    #[must_use]
    pub fn healthy() -> Self {
        Self {
            evaluations: vec![portable_scores(0.75, 0.15, 0.1)],
            input_tokens: 30,
            device: JevDevice::Cpu,
            error: None,
        }
    }

    #[must_use]
    pub fn unavailable() -> Self {
        Self {
            error: Some(JevProviderErrorKind::Unavailable),
            ..Self::healthy()
        }
    }
}

impl JevRemoteTransport for MockJevRemoteTransport {
    fn score(
        &self,
        _model_id: String,
        items: Vec<JevHypothesis>,
        _options: JevExecutionOptions,
    ) -> BoxFuture<'static, Result<RemoteJevScoreBatch, JevProviderError>> {
        let error = self.error;
        let scores = self.evaluations.first().copied().unwrap_or(portable_scores(
            0.5,
            0.25,
            0.25,
        ));
        let input_tokens = self.input_tokens;
        let device = self.device;
        Box::pin(async move {
            if let Some(kind) = error {
                return Err(JevProviderError::new(kind));
            }
            Ok(RemoteJevScoreBatch {
                evaluations: vec![scores; items.len()],
                input_tokens,
                device,
            })
        })
    }
}

/// Scripted Provider used by contract tests and local fallback gates.
#[derive(Clone, Debug)]
pub struct MockJevProvider {
    capabilities: JevProviderCapabilities,
    health: JevHealth,
    scores: JevScores,
    device: JevDevice,
    single_tokens: u64,
    batch_tokens: u64,
    error: Option<JevProviderErrorKind>,
    delay: Duration,
    failures_before_success: usize,
    calls: Arc<AtomicUsize>,
}

impl MockJevProvider {
    /// Healthy mock with default CPU/float32 capabilities.
    #[must_use]
    pub fn healthy(provider_id: impl Into<String>) -> Self {
        let provider_id = provider_id.into();
        Self {
            capabilities: JevProviderCapabilities {
                model_id: format!("{provider_id}-nli"),
                provider_id,
                max_batch_size: 16,
                devices: vec![JevDevice::Cpu],
                dtypes: vec![JevDtype::Float32],
            },
            health: JevHealth::Healthy,
            scores: portable_scores(0.8, 0.1, 0.1),
            device: JevDevice::Cpu,
            single_tokens: 12,
            batch_tokens: 24,
            error: None,
            delay: Duration::ZERO,
            failures_before_success: 0,
            calls: Arc::new(AtomicUsize::new(0)),
        }
    }

    /// Unavailable mock used for fail-open and fallback tests.
    #[must_use]
    pub fn unavailable(provider_id: impl Into<String>) -> Self {
        Self {
            health: JevHealth::Unavailable,
            error: Some(JevProviderErrorKind::Unavailable),
            ..Self::healthy(provider_id)
        }
    }

    #[must_use]
    pub fn with_capabilities(mut self, capabilities: JevProviderCapabilities) -> Self {
        self.capabilities = capabilities;
        self
    }

    #[must_use]
    pub fn with_error(mut self, kind: JevProviderErrorKind) -> Self {
        self.error = Some(kind);
        self.health = match kind {
            JevProviderErrorKind::Unavailable | JevProviderErrorKind::Timeout => {
                JevHealth::Unavailable
            }
            JevProviderErrorKind::ResourceExhausted => JevHealth::Degraded,
            _ => self.health,
        };
        self
    }

    #[must_use]
    pub fn with_delay(mut self, delay: Duration) -> Self {
        self.delay = delay;
        self
    }

    #[must_use]
    pub fn with_failures_before_success(mut self, failures: usize) -> Self {
        self.failures_before_success = failures;
        self
    }

    #[must_use]
    pub fn with_scores(mut self, scores: JevScores) -> Self {
        self.scores = scores;
        self
    }

    #[must_use]
    pub fn with_device(mut self, device: JevDevice) -> Self {
        self.device = device;
        self
    }

    #[must_use]
    pub fn calls(&self) -> usize {
        self.calls.load(Ordering::SeqCst)
    }

    fn outcome(&self) -> Result<(), JevProviderError> {
        let previous = self.calls.fetch_add(1, Ordering::SeqCst);
        if previous < self.failures_before_success {
            return Err(JevProviderError::new(JevProviderErrorKind::Unavailable));
        }
        self.error
            .map_or(Ok(()), |kind| Err(JevProviderError::new(kind)))
    }
}

impl JevProvider for MockJevProvider {
    fn evaluate(
        &self,
        _input: JevHypothesis,
        _options: JevExecutionOptions,
    ) -> BoxFuture<'static, Result<JevEvaluation, JevProviderError>> {
        let this = self.clone();
        Box::pin(async move {
            if !this.delay.is_zero() {
                tokio::time::sleep(this.delay).await;
            }
            this.outcome()?;
            Ok(JevEvaluation {
                scores: this.scores,
                input_tokens: this.single_tokens,
                device: this.device,
            })
        })
    }

    fn batch_evaluate(
        &self,
        inputs: Vec<JevHypothesis>,
        _options: JevExecutionOptions,
    ) -> BoxFuture<'static, Result<JevBatchEvaluation, JevProviderError>> {
        let this = self.clone();
        Box::pin(async move {
            if !this.delay.is_zero() {
                tokio::time::sleep(this.delay).await;
            }
            this.outcome()?;
            Ok(JevBatchEvaluation {
                evaluations: vec![this.scores; inputs.len()],
                input_tokens: this.batch_tokens,
                device: this.device,
            })
        })
    }

    fn health(&self) -> BoxFuture<'static, JevHealth> {
        let health = self.health;
        Box::pin(async move { health })
    }

    fn capabilities(&self) -> JevProviderCapabilities {
        self.capabilities.clone()
    }
}

/// Three capability-distinct mocks used by the Provider contract gate.
#[must_use]
pub fn contract_capability_mocks() -> Vec<MockJevProvider> {
    vec![
        MockJevProvider::healthy("mock-cpu-f32").with_capabilities(JevProviderCapabilities {
            provider_id: "mock-cpu-f32".to_owned(),
            model_id: "contract-nli-cpu".to_owned(),
            max_batch_size: 16,
            devices: vec![JevDevice::Cpu],
            dtypes: vec![JevDtype::Float32],
        }),
        MockJevProvider::healthy("mock-cuda-f16")
            .with_capabilities(JevProviderCapabilities {
                provider_id: "mock-cuda-f16".to_owned(),
                model_id: "contract-nli-cuda".to_owned(),
                max_batch_size: 8,
                devices: vec![JevDevice::Cuda, JevDevice::Cpu],
                dtypes: vec![JevDtype::Float16, JevDtype::Float32],
            })
            .with_device(JevDevice::Cuda),
        MockJevProvider::healthy("mock-mps-bf16")
            .with_capabilities(JevProviderCapabilities {
                provider_id: "mock-mps-bf16".to_owned(),
                model_id: "contract-nli-mps".to_owned(),
                max_batch_size: 4,
                devices: vec![JevDevice::Mps, JevDevice::Cpu],
                dtypes: vec![JevDtype::Bfloat16, JevDtype::Float32],
            })
            .with_device(JevDevice::Mps)
            .with_scores(portable_scores(0.6, 0.25, 0.15)),
    ]
}

/// Builds already-validated portable scores for mocks and parsers.
#[must_use]
pub fn portable_scores(entailment: f32, contradiction: f32, neutral: f32) -> JevScores {
    JevScores::try_new(entailment, contradiction, neutral)
        .unwrap_or(JevScores {
            entailment: 0.0,
            contradiction: 0.0,
            neutral: 1.0,
        })
}

#[allow(
    clippy::cast_possible_truncation,
    reason = "NLI probabilities are validated in [0,1]; f64 JSON scores are narrowed to f32"
)]
fn narrow_score(value: f64) -> f32 {
    value as f32
}

/// Parses Provider-neutral NLI scores without forcing one model JSON schema.
///
/// Accepted shapes:
/// - direct `{entailment, contradiction, neutral}`
/// - `{scores: {...}}` / `{scores: [{...}, ...]}`
/// - `{results: [{scores: {...}} | {labels: [{label, score}, ...]}, ...]}`
///
/// # Errors
///
/// Rejects missing labels, non-finite values, or unnormalized totals.
pub fn parse_jev_scores(value: &serde_json::Value) -> Result<JevScores, JevProviderError> {
    parse_jev_scores_at_depth(value, 3)
}

fn parse_jev_scores_at_depth(
    value: &serde_json::Value,
    depth: u8,
) -> Result<JevScores, JevProviderError> {
    if depth == 0 {
        return Err(JevProviderError::invalid_response());
    }
    if let Some(scores) = score_object(value)? {
        return Ok(scores);
    }
    if let Some(labels) = value.get("labels").and_then(serde_json::Value::as_array) {
        return scores_from_labels(labels);
    }
    if let Some(inner) = value
        .get("scores")
        .filter(|inner| inner.is_object())
        .or_else(|| value.get("result").filter(|inner| inner.is_object()))
        .or_else(|| value.get("output").filter(|inner| inner.is_object()))
    {
        return parse_jev_scores_at_depth(inner, depth.saturating_sub(1));
    }
    Err(JevProviderError::invalid_response())
}

/// Parses a batch of Provider-neutral NLI scores.
///
/// # Errors
///
/// Rejects non-array payloads and any invalid item.
pub fn parse_jev_score_list(value: &serde_json::Value) -> Result<Vec<JevScores>, JevProviderError> {
    if let Some(list) = value.as_array() {
        return list.iter().map(parse_jev_scores).collect();
    }
    if let Some(list) = value
        .get("scores")
        .and_then(serde_json::Value::as_array)
        .or_else(|| value.get("results").and_then(serde_json::Value::as_array))
    {
        return list.iter().map(parse_jev_scores).collect();
    }
    if value.get("entailment").is_some() || value.get("labels").is_some() {
        return Ok(vec![parse_jev_scores(value)?]);
    }
    if let Some(inner) = value.get("scores") {
        return Ok(vec![parse_jev_scores(inner)?]);
    }
    Err(JevProviderError::invalid_response())
}

fn score_object(value: &serde_json::Value) -> Result<Option<JevScores>, JevProviderError> {
    let (Some(entailment), Some(contradiction), Some(neutral)) = (
        value.get("entailment").and_then(serde_json::Value::as_f64),
        value
            .get("contradiction")
            .and_then(serde_json::Value::as_f64),
        value.get("neutral").and_then(serde_json::Value::as_f64),
    ) else {
        return Ok(None);
    };
    Ok(Some(JevScores::try_new(
        narrow_score(entailment),
        narrow_score(contradiction),
        narrow_score(neutral),
    )?))
}

fn scores_from_labels(labels: &[serde_json::Value]) -> Result<JevScores, JevProviderError> {
    let mut entailment = None;
    let mut contradiction = None;
    let mut neutral = None;
    for label in labels {
        let name = label
            .get("label")
            .and_then(serde_json::Value::as_str)
            .unwrap_or_default()
            .to_ascii_lowercase();
        let score = narrow_score(
            label
                .get("score")
                .and_then(serde_json::Value::as_f64)
                .ok_or_else(JevProviderError::invalid_response)?,
        );
        match name.as_str() {
            "entailment" | "entails" | "yes" => entailment = Some(score),
            "contradiction" | "contradicts" | "no" => contradiction = Some(score),
            "neutral" | "unknown" => neutral = Some(score),
            _ => {}
        }
    }
    let (Some(entailment), Some(contradiction), Some(neutral)) =
        (entailment, contradiction, neutral)
    else {
        return Err(JevProviderError::invalid_response());
    };
    JevScores::try_new(entailment, contradiction, neutral)
}

fn openjev_capabilities(provider_id: &str, model_id: String) -> JevProviderCapabilities {
    JevProviderCapabilities {
        provider_id: provider_id.to_owned(),
        model_id,
        max_batch_size: 8,
        devices: vec![JevDevice::Cpu, JevDevice::Cuda, JevDevice::Mps],
        dtypes: vec![
            JevDtype::Float32,
            JevDtype::Float16,
            JevDtype::Bfloat16,
            JevDtype::Int8,
            JevDtype::Int4,
        ],
    }
}

fn resolve_device(requested: JevDevice, reported: JevDevice) -> JevDevice {
    match requested {
        JevDevice::Auto => {
            if reported == JevDevice::Auto {
                JevDevice::Cpu
            } else {
                reported
            }
        }
        other => other,
    }
}

fn parse_device(value: &str) -> Option<JevDevice> {
    match value.trim().to_ascii_lowercase().as_str() {
        "auto" => Some(JevDevice::Auto),
        "cuda" | "gpu" => Some(JevDevice::Cuda),
        "mps" => Some(JevDevice::Mps),
        "cpu" => Some(JevDevice::Cpu),
        _ => None,
    }
}

fn parse_dtype(value: &str) -> Option<JevDtype> {
    match value.trim().to_ascii_lowercase().as_str() {
        "auto" => Some(JevDtype::Auto),
        "float32" | "fp32" => Some(JevDtype::Float32),
        "float16" | "fp16" => Some(JevDtype::Float16),
        "bfloat16" | "bf16" => Some(JevDtype::Bfloat16),
        "int8" => Some(JevDtype::Int8),
        "int4" => Some(JevDtype::Int4),
        _ => None,
    }
}

fn device_name(device: JevDevice) -> &'static str {
    match device {
        JevDevice::Auto => "auto",
        JevDevice::Cuda => "cuda",
        JevDevice::Mps => "mps",
        JevDevice::Cpu => "cpu",
    }
}

fn dtype_name(dtype: JevDtype) -> &'static str {
    match dtype {
        JevDtype::Auto => "auto",
        JevDtype::Float32 => "float32",
        JevDtype::Float16 => "float16",
        JevDtype::Bfloat16 => "bfloat16",
        JevDtype::Int8 => "int8",
        JevDtype::Int4 => "int4",
    }
}

fn estimate_tokens(items: &[JevHypothesis]) -> u64 {
    items
        .iter()
        .map(|item| {
            let words = item.premise.split_whitespace().count()
                + item.hypothesis.split_whitespace().count();
            u64::try_from(words).unwrap_or(u64::MAX)
        })
        .sum()
}

fn valid_identity(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 128
        && value.trim() == value
        && !value.chars().any(char::is_control)
}

/// Serialized remote scoring request used by hosts that proxy Jev themselves.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct JevRemoteScoreRequest {
    pub model: String,
    pub device: String,
    pub dtype: String,
    pub items: Vec<JevRemoteScoreItem>,
}

/// One premise/hypothesis pair in a remote scoring request.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct JevRemoteScoreItem {
    pub premise: String,
    pub hypothesis: String,
}

#[cfg(test)]
mod tests {
    use std::future::Future;

    use super::*;

    #[derive(Debug)]
    struct ScriptedLocalRuntime {
        available: bool,
        capabilities: JevProviderCapabilities,
        scores: JevScores,
        device: JevDevice,
    }

    impl ScriptedLocalRuntime {
        fn openjev() -> Self {
            Self {
                available: true,
                capabilities: openjev_capabilities("openjev-local", OPENJEV_DEFAULT_MODEL_ID.to_owned()),
                scores: portable_scores(0.7, 0.2, 0.1),
                device: JevDevice::Cpu,
            }
        }
    }

    impl OpenJevLocalRuntime for ScriptedLocalRuntime {
        fn score(
            &self,
            inputs: Vec<JevHypothesis>,
            _options: JevExecutionOptions,
        ) -> BoxFuture<'static, Result<JevBatchEvaluation, JevProviderError>> {
            let available = self.available;
            let scores = self.scores;
            let device = self.device;
            Box::pin(async move {
                if !available {
                    return Err(JevProviderError::new(JevProviderErrorKind::Unavailable));
                }
                Ok(JevBatchEvaluation {
                    evaluations: vec![scores; inputs.len()],
                    input_tokens: 18,
                    device,
                })
            })
        }

        fn health(&self) -> JevHealth {
            if self.available {
                JevHealth::Healthy
            } else {
                JevHealth::Unavailable
            }
        }

        fn capabilities(&self) -> JevProviderCapabilities {
            self.capabilities.clone()
        }
    }

    #[derive(Debug)]
    struct ScriptedRemoteTransport {
        mode: RemoteMode,
        scores: JevScores,
        device: JevDevice,
        tokens: u64,
    }

    #[derive(Clone, Copy, Debug)]
    enum RemoteMode {
        Success,
        Unavailable,
        InvalidResponse,
    }

    impl JevRemoteTransport for ScriptedRemoteTransport {
        fn score(
            &self,
            _model_id: String,
            items: Vec<JevHypothesis>,
            _options: JevExecutionOptions,
        ) -> BoxFuture<'static, Result<RemoteJevScoreBatch, JevProviderError>> {
            let mode = self.mode;
            let scores = self.scores;
            let device = self.device;
            let tokens = self.tokens;
            Box::pin(async move {
                match mode {
                    RemoteMode::Success => Ok(RemoteJevScoreBatch {
                        evaluations: vec![scores; items.len()],
                        input_tokens: tokens,
                        device,
                    }),
                    RemoteMode::Unavailable => {
                        Err(JevProviderError::new(JevProviderErrorKind::Unavailable))
                    }
                    RemoteMode::InvalidResponse => Ok(RemoteJevScoreBatch {
                        evaluations: Vec::new(),
                        input_tokens: tokens,
                        device,
                    }),
                }
            })
        }
    }

    fn hypothesis() -> JevHypothesis {
        JevHypothesis {
            premise: "tests passed".to_owned(),
            hypothesis: "the change is verified".to_owned(),
        }
    }

    fn options() -> JevExecutionOptions {
        JevExecutionOptions {
            device: JevDevice::Cpu,
            dtype: JevDtype::Float32,
        }
    }

    fn auto_options() -> JevExecutionOptions {
        JevExecutionOptions {
            device: JevDevice::Auto,
            dtype: JevDtype::Auto,
        }
    }

    fn runtime(providers: Vec<Arc<dyn JevProvider>>) -> JevRuntime {
        runtime_with(
            providers,
            JevRuntimeConfig {
                timeout: Duration::from_millis(50),
                retries: 0,
            },
        )
    }

    fn runtime_with(providers: Vec<Arc<dyn JevProvider>>, config: JevRuntimeConfig) -> JevRuntime {
        JevRuntime::new(providers, config)
    }

    fn block_on<F: Future>(future: F) -> F::Output {
        tokio::runtime::Builder::new_current_thread()
            .enable_time()
            .build()
            .expect("runtime")
            .block_on(future)
    }

    fn remote_config() -> OpenJevRemoteConfig {
        OpenJevRemoteConfig::try_new(OpenJevRemoteConfigRequest {
            provider_id: "openjev-remote".to_owned(),
            endpoint: "https://nli.example.com/v1/score".to_owned(),
            model_id: OPENJEV_DEFAULT_MODEL_ID.to_owned(),
            max_batch_size: 8,
            devices: vec![JevDevice::Cpu, JevDevice::Cuda],
            dtypes: vec![JevDtype::Float32, JevDtype::Float16],
            timeout: Duration::from_millis(200),
            api_key: Some("remote-secret".to_owned()),
        })
        .expect("remote config")
    }

    fn remote_provider(mode: RemoteMode) -> OpenJevRemoteProvider {
        OpenJevRemoteProvider::new(
            remote_config(),
            Arc::new(ScriptedRemoteTransport {
                mode,
                scores: JevScores::try_new(0.75, 0.15, 0.1).expect("valid score"),
                device: JevDevice::Cpu,
                tokens: 30,
            }),
        )
    }

    #[test]
    fn three_capability_mocks_satisfy_provider_contract() {
        block_on(async {
            for mock in contract_capability_mocks() {
                let capabilities = mock.capabilities();
                assert!(!capabilities.provider_id.is_empty());
                assert!(!capabilities.model_id.is_empty());
                assert!(capabilities.max_batch_size > 0);
                assert!(!capabilities.devices.is_empty());
                assert!(!capabilities.dtypes.is_empty());

                let device = capabilities.devices[0];
                let dtype = capabilities.dtypes[0];
                let options = JevExecutionOptions { device, dtype };
                assert_eq!(mock.health().await, JevHealth::Healthy);

                let single = mock.evaluate(hypothesis(), options).await.expect("evaluate");
                assert!(single.scores.valid());
                assert_ne!(single.device, JevDevice::Auto);
                assert!(single.input_tokens > 0);

                let batch_size = capabilities.max_batch_size.min(2);
                let batch = mock
                    .batch_evaluate(vec![hypothesis(); batch_size], options)
                    .await
                    .expect("batch");
                assert_eq!(batch.evaluations.len(), batch_size);
                assert_ne!(batch.device, JevDevice::Auto);
            }
        });
    }

    #[test]
    fn capability_gate_skips_unsupported_batch_and_device() {
        block_on(async {
            let mock = contract_capability_mocks()
                .into_iter()
                .next()
                .expect("cpu mock");
            let oversized = runtime(vec![Arc::new(mock)])
                .batch_evaluate(vec![hypothesis(); 32], options())
                .await;
            assert!(oversized.value.is_none());
            assert_eq!(oversized.failures[0].kind, JevProviderErrorKind::Unsupported);

            let gpu = runtime(vec![Arc::new(
                MockJevProvider::healthy("cpu-only").with_capabilities(JevProviderCapabilities {
                    provider_id: "cpu-only".to_owned(),
                    model_id: "cpu-only-nli".to_owned(),
                    max_batch_size: 4,
                    devices: vec![JevDevice::Cpu],
                    dtypes: vec![JevDtype::Float32],
                }),
            )])
            .evaluate(
                hypothesis(),
                JevExecutionOptions {
                    device: JevDevice::Cuda,
                    dtype: JevDtype::Float32,
                },
            )
            .await;
            assert!(gpu.value.is_none());
            assert_eq!(gpu.failures[0].kind, JevProviderErrorKind::Unsupported);
        });
    }

    #[test]
    fn local_openjev_runtime_gap_reports_unavailable_and_fails_open() {
        block_on(async {
            let local = OpenJevLocalProvider::with_runtime_gap();
            assert!(!local.has_runtime());
            assert_eq!(local.health().await, JevHealth::Unavailable);
            assert_eq!(
                local.evaluate(hypothesis(), options()).await.unwrap_err().kind(),
                JevProviderErrorKind::Unavailable
            );
            assert!(OPENJEV_LOCAL_RUNTIME_GAP.contains("not embedded"));

            let remote = remote_provider(RemoteMode::Success);
            let ordered = jev_provider_order(
                JevFallbackPolicy::LocalFirst,
                Some(Arc::new(local)),
                Some(Arc::new(remote)),
            );
            assert_eq!(ordered.len(), 2);
            let outcome = runtime(ordered).evaluate(hypothesis(), options()).await;
            let evaluation = outcome.value.expect("remote fallback");
            assert_eq!(outcome.observation.expect("observation").provider_id, "openjev-remote");
            assert!((evaluation.scores.entailment - 0.75).abs() < f32::EPSILON);
        });
    }

    #[test]
    fn local_openjev_with_injected_runtime_scores_and_observes() {
        block_on(async {
            let local = OpenJevLocalProvider::with_runtime(Arc::new(ScriptedLocalRuntime::openjev()));
            assert!(local.has_runtime());
            assert_eq!(local.health().await, JevHealth::Healthy);
            let runtime = runtime(jev_provider_order(
                JevFallbackPolicy::LocalOnly,
                Some(Arc::new(local)),
                None,
            ));
            let outcome = runtime.evaluate(hypothesis(), auto_options()).await;
            let observation = outcome.observation.expect("observation");
            assert_eq!(observation.provider_id, "openjev-local");
            assert_eq!(observation.model_id, OPENJEV_DEFAULT_MODEL_ID);
            assert_eq!(observation.device, JevDevice::Cpu);
            assert_eq!(observation.batch_size, 1);
            assert!(observation.input_tokens > 0);
            assert!((observation.confidence - 0.7).abs() < f32::EPSILON);
        });
    }

    #[test]
    fn remote_first_policy_prefers_remote_then_local() {
        block_on(async {
            let local = OpenJevLocalProvider::with_runtime(Arc::new(ScriptedLocalRuntime::openjev()));
            let remote = remote_provider(RemoteMode::Success);
            let ordered = jev_provider_order(
                JevFallbackPolicy::RemoteFirst,
                Some(Arc::new(local)),
                Some(Arc::new(remote)),
            );
            let outcome = runtime(ordered).evaluate(hypothesis(), options()).await;
            assert_eq!(
                outcome.observation.expect("observation").provider_id,
                "openjev-remote"
            );
        });
    }

    #[test]
    fn skip_policy_never_stops_the_coding_agent_worker() {
        block_on(async {
            let ordered = jev_provider_order(JevFallbackPolicy::Skip, None, None);
            assert!(ordered.is_empty());
            let runtime = runtime(ordered);
            let outcome = runtime.batch_evaluate(vec![hypothesis(), hypothesis()], options()).await;
            assert!(outcome.value.is_none());
            assert!(outcome.observation.is_none());
            // Worker step still completes; Jev is optional context, not a gate.
            let worker_continued = outcome.value.is_none();
            assert!(worker_continued);
        });
    }

    #[test]
    fn remote_outage_falls_back_to_local_or_skips() {
        block_on(async {
            let local = OpenJevLocalProvider::with_runtime(Arc::new(ScriptedLocalRuntime::openjev()));
            let remote = remote_provider(RemoteMode::Unavailable);
            let ordered = jev_provider_order(
                JevFallbackPolicy::RemoteFirst,
                Some(Arc::new(local)),
                Some(Arc::new(remote)),
            );
            let outcome = runtime(ordered).evaluate(hypothesis(), options()).await;
            assert_eq!(
                outcome.observation.expect("observation").provider_id,
                "openjev-local"
            );
            assert_eq!(outcome.failures.len(), 1);
            assert_eq!(
                outcome.failures[0].kind,
                JevProviderErrorKind::Unavailable
            );

            let both_broken = jev_provider_order(
                JevFallbackPolicy::Fallback,
                Some(Arc::new(OpenJevLocalProvider::with_runtime_gap())),
                Some(Arc::new(remote_provider(RemoteMode::Unavailable))),
            );
            let outcome = runtime(both_broken).evaluate(hypothesis(), options()).await;
            assert!(outcome.value.is_none());
            assert!(outcome.failures.len() >= 2);
        });
    }

    #[test]
    fn transient_remote_failure_is_retried_then_succeeds() {
        block_on(async {
            let remote = Arc::new(
                MockJevProvider::healthy("remote-flaky").with_failures_before_success(1),
            );
            let runtime = runtime_with(
                vec![remote.clone()],
                JevRuntimeConfig {
                    timeout: Duration::from_millis(200),
                    retries: 1,
                },
            );
            let outcome = runtime.evaluate(hypothesis(), options()).await;
            assert!(outcome.value.is_some());
            assert_eq!(outcome.failures.len(), 1);
            assert_eq!(remote.calls(), 2);
        });
    }

    #[test]
    fn provider_timeout_is_recorded_and_fail_opens() {
        block_on(async {
            let slow = Arc::new(MockJevProvider::healthy("slow").with_delay(Duration::from_millis(80)));
            let outcome = runtime(vec![slow]).evaluate(hypothesis(), options()).await;
            assert!(outcome.value.is_none());
            assert_eq!(outcome.failures[0].kind, JevProviderErrorKind::Timeout);
        });
    }

    #[test]
    fn invalid_remote_payload_is_not_accepted_as_a_decision() {
        block_on(async {
            let outcome = runtime(vec![Arc::new(remote_provider(RemoteMode::InvalidResponse))])
                .evaluate(hypothesis(), options())
                .await;
            assert!(outcome.value.is_none());
            assert_eq!(
                outcome.failures[0].kind,
                JevProviderErrorKind::InvalidResponse
            );
        });
    }

    #[test]
    fn provider_is_replaceable_and_falls_back_with_complete_observation() {
        block_on(async {
            let outcome = runtime(vec![
                Arc::new(MockJevProvider::unavailable("local")),
                Arc::new(MockJevProvider::healthy("remote")),
            ])
            .batch_evaluate(vec![hypothesis(), hypothesis()], options())
            .await;

            assert_eq!(outcome.value.expect("fallback result").evaluations.len(), 2);
            assert_eq!(outcome.failures.len(), 1);
            let observation = outcome.observation.expect("observation");
            assert_eq!(observation.provider_id, "remote");
            assert_eq!(observation.input_tokens, 24);
            assert_eq!(observation.batch_size, 2);
            assert_eq!(observation.device, JevDevice::Cpu);
            assert!((observation.confidence - 0.8).abs() < f32::EPSILON);
        });
    }

    #[test]
    fn total_provider_failure_skips_jev_instead_of_failing_the_caller() {
        block_on(async {
            let outcome = runtime(vec![Arc::new(MockJevProvider::unavailable("broken"))])
                .evaluate(hypothesis(), options())
                .await;

            assert!(outcome.value.is_none());
            assert!(outcome.observation.is_none());
            assert_eq!(outcome.failures[0].kind, JevProviderErrorKind::Unavailable);
        });
    }

    #[test]
    fn remote_config_rejects_non_https_and_redacts_secret() {
        assert!(OpenJevRemoteConfig::try_new(OpenJevRemoteConfigRequest {
            provider_id: "openjev-remote".to_owned(),
            endpoint: "http://nli.example.com/v1/score".to_owned(),
            model_id: OPENJEV_DEFAULT_MODEL_ID.to_owned(),
            max_batch_size: 8,
            devices: vec![JevDevice::Cpu],
            dtypes: vec![JevDtype::Float32],
            timeout: Duration::from_millis(200),
            api_key: None,
        })
        .is_err());

        let config = remote_config();
        let debug = format!("{config:?}");
        assert!(debug.contains("[REDACTED]"));
        assert!(!debug.contains("remote-secret"));
    }

    #[test]
    fn remote_settings_parse_provider_endpoint_key_timeout_retries() {
        let settings = OpenJevRemoteSettings::from_toml(
            r#"
providerId = "openjev-remote"
endpoint = "https://nli.example.com/v1/score"
apiKey = "settings-secret"
modelId = "openjev-4b-v2"
timeoutMs = 250
retries = 2
maxBatchSize = 8
devices = ["cpu", "cuda"]
dtypes = ["float32", "float16"]
"#,
        )
        .expect("settings");
        let (config, runtime_config) = settings.to_config_and_runtime().expect("converted");
        assert_eq!(config.provider_id(), "openjev-remote");
        assert_eq!(config.endpoint(), "https://nli.example.com/v1/score");
        assert_eq!(config.timeout(), Duration::from_millis(250));
        assert_eq!(runtime_config.retries, 2);
        assert!(!format!("{config:?}").contains("settings-secret"));
    }

    #[test]
    fn flexible_nli_payload_parse_does_not_force_one_json_schema() {
        let direct = serde_json::json!({
            "entailment": 0.7,
            "contradiction": 0.2,
            "neutral": 0.1
        });
        let labeled = serde_json::json!({
            "results": [{
                "labels": [
                    {"label": "entailment", "score": 0.7},
                    {"label": "contradiction", "score": 0.2},
                    {"label": "neutral", "score": 0.1}
                ]
            }]
        });
        let nested = serde_json::json!({
            "scores": [
                {"entailment": 0.7, "contradiction": 0.2, "neutral": 0.1},
                {"labels": [
                    {"label": "ENTAILMENT", "score": 0.55},
                    {"label": "contradiction", "score": 0.05},
                    {"label": "neutral", "score": 0.40}
                ]}
            ],
            "input_tokens": 9,
            "device": "cuda"
        });

        assert_eq!(
            parse_jev_scores(&direct).expect("direct"),
            JevScores::try_new(0.7, 0.2, 0.1).expect("valid")
        );
        assert_eq!(
            parse_jev_score_list(&labeled).expect("labeled"),
            vec![JevScores::try_new(0.7, 0.2, 0.1).expect("valid")]
        );
        let batch = parse_jev_score_list(&nested).expect("nested");
        assert_eq!(batch.len(), 2);
        assert!((batch[1].entailment - 0.55).abs() < f32::EPSILON);
    }
}
