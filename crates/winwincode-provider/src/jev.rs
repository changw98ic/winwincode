// SPDX-License-Identifier: Apache-2.0

//! Provider-neutral hypothesis scoring used by the context runtime.

use std::{fmt, sync::Arc, time::Duration};

use futures::future::BoxFuture;
use tokio::time::{Instant, timeout};

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

#[cfg(test)]
mod tests {
    use super::*;

    #[derive(Clone, Copy, Debug)]
    enum MockMode {
        Success,
        Unavailable,
    }

    #[derive(Debug)]
    struct MockProvider {
        id: &'static str,
        mode: MockMode,
    }

    impl MockProvider {
        fn score() -> JevScores {
            JevScores::try_new(0.8, 0.1, 0.1).expect("valid score")
        }
    }

    impl JevProvider for MockProvider {
        fn evaluate(
            &self,
            _input: JevHypothesis,
            _options: JevExecutionOptions,
        ) -> BoxFuture<'static, Result<JevEvaluation, JevProviderError>> {
            let mode = self.mode;
            Box::pin(async move {
                match mode {
                    MockMode::Success => Ok(JevEvaluation {
                        scores: Self::score(),
                        input_tokens: 12,
                        device: JevDevice::Cpu,
                    }),
                    MockMode::Unavailable => {
                        Err(JevProviderError::new(JevProviderErrorKind::Unavailable))
                    }
                }
            })
        }

        fn batch_evaluate(
            &self,
            inputs: Vec<JevHypothesis>,
            _options: JevExecutionOptions,
        ) -> BoxFuture<'static, Result<JevBatchEvaluation, JevProviderError>> {
            let mode = self.mode;
            Box::pin(async move {
                match mode {
                    MockMode::Success => Ok(JevBatchEvaluation {
                        evaluations: vec![Self::score(); inputs.len()],
                        input_tokens: 24,
                        device: JevDevice::Cpu,
                    }),
                    MockMode::Unavailable => {
                        Err(JevProviderError::new(JevProviderErrorKind::Unavailable))
                    }
                }
            })
        }

        fn health(&self) -> BoxFuture<'static, JevHealth> {
            let health = match self.mode {
                MockMode::Success => JevHealth::Healthy,
                MockMode::Unavailable => JevHealth::Unavailable,
            };
            Box::pin(async move { health })
        }

        fn capabilities(&self) -> JevProviderCapabilities {
            JevProviderCapabilities {
                provider_id: self.id.to_owned(),
                model_id: "mock-nli".to_owned(),
                max_batch_size: 16,
                devices: vec![JevDevice::Cpu],
                dtypes: vec![JevDtype::Float32],
            }
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

    fn runtime(providers: Vec<Arc<dyn JevProvider>>) -> JevRuntime {
        JevRuntime::new(
            providers,
            JevRuntimeConfig {
                timeout: Duration::from_secs(1),
                retries: 0,
            },
        )
    }

    #[test]
    fn provider_is_replaceable_and_falls_back_with_complete_observation() {
        tokio::runtime::Builder::new_current_thread()
            .enable_time()
            .build()
            .expect("runtime")
            .block_on(async {
                let outcome = runtime(vec![
                    Arc::new(MockProvider {
                        id: "local",
                        mode: MockMode::Unavailable,
                    }),
                    Arc::new(MockProvider {
                        id: "remote",
                        mode: MockMode::Success,
                    }),
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
        tokio::runtime::Builder::new_current_thread()
            .enable_time()
            .build()
            .expect("runtime")
            .block_on(async {
                let outcome = runtime(vec![Arc::new(MockProvider {
                    id: "broken",
                    mode: MockMode::Unavailable,
                })])
                .evaluate(hypothesis(), options())
                .await;

                assert!(outcome.value.is_none());
                assert!(outcome.observation.is_none());
                assert_eq!(outcome.failures[0].kind, JevProviderErrorKind::Unavailable);
            });
    }
}
