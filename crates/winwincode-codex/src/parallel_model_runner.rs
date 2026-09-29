// SPDX-License-Identifier: Apache-2.0

//! Parallel multi-model orchestration for the Fusion Runtime (FUSION-02).
//!
//! Fusion Blind Panel targets (Astra / Sol / Fable / Opus / GLM / Kimi / Qwen
//! product abstractions) execute concurrently over the existing kernel
//! [`ModelPort`]. This module is a Fusion consumer only:
//!
//! - Provider routing, credentials, and the Device provider secrets path stay
//!   inside the `WinWinCode` Provider Runtime (ge4r) behind [`ModelPort`].
//! - Reasoning effort is carried in each attempt's existing
//!   [`ModelPortRequest::payload_json`] contract; this runner never rewrites
//!   the model request schema.
//! - One model failure never fails the whole Fusion batch. Each target keeps
//!   an independent terminal status and keeps sibling successes intact.
//!
//! Capabilities owned here: attempt timeout, cancel, partial success, shared
//! token/cost/attempt budget, ordered provider fallback, cost capture, and
//! latency capture.

use std::{
    collections::HashSet,
    fmt,
    future::pending,
    sync::{
        Arc,
        atomic::{AtomicBool, AtomicU64, Ordering},
    },
    time::{Duration, Instant},
};

use futures::{StreamExt as _, stream::FuturesUnordered};
use serde_json::Value;
use tokio::sync::watch;
use winwincode_kernel::{ModelPort, ModelPortFailure, ModelPortRequest};
use winwincode_provider::ProviderTokenUsage;

/// One Provider/model route. The first route is primary; later routes are fallbacks.
#[derive(Clone, Debug)]
pub struct ParallelModelAttempt {
    pub route: String,
    pub request: ModelPortRequest,
}

/// One independently executed model target.
#[derive(Clone, Debug)]
pub struct ParallelModelTarget {
    pub target_id: String,
    pub attempts: Vec<ParallelModelAttempt>,
}

/// Product-level Fusion panel model identities.
///
/// These strings are Fusion target identities only. They do not open Provider
/// connections, invent capabilities, or carry credentials.
pub mod fusion_panel {
    /// Canonical Fusion panel identity.
    pub const ASTRA: &str = "astra";
    /// Canonical Fusion panel identity.
    pub const SOL: &str = "sol";
    /// Canonical Fusion panel identity.
    pub const FABLE: &str = "fable";
    /// Canonical Fusion panel identity.
    pub const OPUS: &str = "opus";
    /// Canonical Fusion panel identity.
    pub const GLM: &str = "glm";
    /// Canonical Fusion panel identity.
    pub const KIMI: &str = "kimi";
    /// Canonical Fusion panel identity.
    pub const QWEN: &str = "qwen";

    /// Default blind-panel target identities.
    #[must_use]
    pub fn default_panel() -> [&'static str; 7] {
        [ASTRA, SOL, FABLE, OPUS, GLM, KIMI, QWEN]
    }
}

/// One panel seat: product model identity plus ordered Provider Runtime routes.
#[derive(Clone, Debug)]
pub struct FusionPanelSeat {
    pub model: String,
    pub routes: Vec<String>,
}

impl FusionPanelSeat {
    /// Builds one blind-panel target for this seat.
    #[must_use]
    pub fn to_target<F>(&self, request_id_prefix: &str, payload_for_route: F) -> ParallelModelTarget
    where
        F: Fn(&str) -> String,
    {
        ParallelModelTarget {
            target_id: self.model.clone(),
            attempts: self
                .routes
                .iter()
                .enumerate()
                .map(|(index, route)| ParallelModelAttempt {
                    route: route.clone(),
                    request: ModelPortRequest {
                        request_id: format!("{request_id_prefix}:{}:{index}", self.model),
                        payload_json: payload_for_route(route),
                    },
                })
                .collect(),
        }
    }
}

/// Optional shared limits for one parallel batch. `None` means no limit.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct ParallelModelBudget {
    pub max_attempts: Option<u64>,
    pub max_total_tokens: Option<u64>,
    pub max_total_cost_micros: Option<u64>,
    pub max_wall_time: Option<Duration>,
    pub attempt_timeout: Option<Duration>,
}

/// A clonable cancellation sender for one batch.
#[derive(Clone, Debug)]
pub struct ParallelModelCancelHandle(watch::Sender<bool>);

/// The receiving half passed to [`ParallelModelRunner::run`].
#[derive(Clone, Debug)]
pub struct ParallelModelCancelSignal(watch::Receiver<bool>);

/// Creates an isolated cancellation pair for one parallel batch.
#[must_use]
pub fn parallel_model_cancellation() -> (ParallelModelCancelHandle, ParallelModelCancelSignal) {
    let (sender, receiver) = watch::channel(false);
    (
        ParallelModelCancelHandle(sender),
        ParallelModelCancelSignal(receiver),
    )
}

impl ParallelModelCancelHandle {
    /// Cancels pending opens and drops every active model stream.
    pub fn cancel(&self) {
        let _ = self.0.send(true);
    }
}

/// Stable terminal state for one target.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ParallelModelStatus {
    Succeeded,
    Failed,
    TimedOut,
    Cancelled,
    BudgetExceeded,
}

/// Metrics and failure facts for one primary or fallback attempt.
#[derive(Clone, Debug)]
pub struct ParallelModelAttemptRecord {
    pub route: String,
    pub latency: Duration,
    pub usage: Option<ProviderTokenUsage>,
    pub cost_micros: Option<u64>,
    pub failure: Option<ModelPortFailure>,
}

/// Independent result for one target. Failed siblings never erase this result.
#[derive(Clone, Debug)]
pub struct ParallelModelResult {
    pub target_id: String,
    pub status: ParallelModelStatus,
    pub selected_route: Option<String>,
    pub frames: Vec<String>,
    pub attempts: Vec<ParallelModelAttemptRecord>,
    pub latency: Duration,
}

/// FUSION compose-seam feed rows: `(candidate_id/target_id, frames)` for each
/// succeeded target.
///
/// Control-plane `fusion_compose::answers_from_parallel_model_frames` consumes
/// these rows. `target_id` is the Fusion `candidate_id`. Failed / timed-out /
/// cancelled / budget-exceeded targets are omitted so one model failure never
/// fails the Fusion compose batch.
#[must_use]
pub fn fusion_compose_frame_rows(batch: &ParallelModelBatchResult) -> Vec<(String, Vec<String>)> {
    batch
        .results
        .iter()
        .filter(|result| result.status == ParallelModelStatus::Succeeded)
        .map(|result| (result.target_id.clone(), result.frames.clone()))
        .collect()
}

/// Measured aggregate facts for the completed portion of a batch.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct ParallelModelTotals {
    pub attempts: u64,
    pub input_tokens: u64,
    pub cached_input_tokens: u64,
    pub cache_write_input_tokens: u64,
    pub output_tokens: u64,
    pub reasoning_output_tokens: u64,
    pub known_cost_micros: u64,
    pub cost_observations: u64,
}

/// Ordered target results plus aggregate usage, cost, and latency.
#[derive(Clone, Debug)]
pub struct ParallelModelBatchResult {
    pub results: Vec<ParallelModelResult>,
    pub totals: ParallelModelTotals,
    pub latency: Duration,
}

/// Invalid runner input. Model and Provider failures are returned per target instead.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ParallelModelRunError;

impl fmt::Display for ParallelModelRunError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("parallel model runner input is invalid")
    }
}

impl std::error::Error for ParallelModelRunError {}

/// Concurrent primary/fallback execution over the canonical model port.
#[derive(Debug)]
pub struct ParallelModelRunner {
    port: Arc<dyn ModelPort>,
}

impl ParallelModelRunner {
    #[must_use]
    pub fn new(port: Arc<dyn ModelPort>) -> Self {
        Self { port }
    }

    /// Runs all targets concurrently and preserves partial successes.
    ///
    /// # Errors
    ///
    /// Rejects empty or duplicate target identities, empty fallback lists, and
    /// zero-valued limits before opening a Provider request.
    pub async fn run(
        &self,
        targets: Vec<ParallelModelTarget>,
        budget: ParallelModelBudget,
        cancel: ParallelModelCancelSignal,
    ) -> Result<ParallelModelBatchResult, ParallelModelRunError> {
        validate(&targets, budget)?;
        let started = Instant::now();
        let tracker = Arc::new(BudgetTracker::new(budget));
        let mut pending = FuturesUnordered::new();
        let target_count = targets.len();
        for (index, target) in targets.into_iter().enumerate() {
            pending.push(run_target(
                index,
                target,
                Arc::clone(&self.port),
                Arc::clone(&tracker),
                started,
                cancel.0.clone(),
            ));
        }
        let mut results = Vec::with_capacity(target_count);
        // Members observe stop signals themselves so their attempt records survive.
        while let Some(result) = pending.next().await {
            results.push(result);
        }
        results.sort_unstable_by_key(|result| result.index);

        Ok(ParallelModelBatchResult {
            results: results.into_iter().map(|result| result.result).collect(),
            totals: tracker.totals(),
            latency: started.elapsed(),
        })
    }
}

struct IndexedResult {
    index: usize,
    result: ParallelModelResult,
}

struct BudgetTracker {
    budget: ParallelModelBudget,
    exhausted: AtomicBool,
    attempts: AtomicU64,
    input_tokens: AtomicU64,
    cached_input_tokens: AtomicU64,
    cache_write_input_tokens: AtomicU64,
    output_tokens: AtomicU64,
    reasoning_output_tokens: AtomicU64,
    known_cost_micros: AtomicU64,
    cost_observations: AtomicU64,
}

impl BudgetTracker {
    fn new(budget: ParallelModelBudget) -> Self {
        Self {
            budget,
            exhausted: AtomicBool::new(false),
            attempts: AtomicU64::new(0),
            input_tokens: AtomicU64::new(0),
            cached_input_tokens: AtomicU64::new(0),
            cache_write_input_tokens: AtomicU64::new(0),
            output_tokens: AtomicU64::new(0),
            reasoning_output_tokens: AtomicU64::new(0),
            known_cost_micros: AtomicU64::new(0),
            cost_observations: AtomicU64::new(0),
        }
    }

    fn reserve_attempt(&self) -> bool {
        if self.exhausted.load(Ordering::Acquire) {
            return false;
        }
        self.attempts
            .fetch_update(Ordering::AcqRel, Ordering::Acquire, |attempts| {
                if self
                    .budget
                    .max_attempts
                    .is_none_or(|limit| attempts < limit)
                {
                    attempts.checked_add(1)
                } else {
                    None
                }
            })
            .is_ok()
    }

    fn record(&self, usage: Option<ProviderTokenUsage>, cost_micros: Option<u64>) {
        if let Some(usage) = usage {
            saturating_add(&self.input_tokens, usage.input_tokens);
            saturating_add(&self.cached_input_tokens, usage.cached_input_tokens);
            saturating_add(
                &self.cache_write_input_tokens,
                usage.cache_write_input_tokens,
            );
            saturating_add(&self.output_tokens, usage.output_tokens);
            saturating_add(&self.reasoning_output_tokens, usage.reasoning_output_tokens);
        }
        if let Some(cost_micros) = cost_micros {
            saturating_add(&self.known_cost_micros, cost_micros);
            saturating_add(&self.cost_observations, 1);
        }
        let total_tokens = self
            .input_tokens
            .load(Ordering::Acquire)
            .saturating_add(self.output_tokens.load(Ordering::Acquire));
        if self
            .budget
            .max_total_tokens
            .is_some_and(|limit| total_tokens > limit)
            || self
                .budget
                .max_total_cost_micros
                .is_some_and(|limit| self.known_cost_micros.load(Ordering::Acquire) > limit)
        {
            self.exhausted.store(true, Ordering::Release);
        }
    }

    fn totals(&self) -> ParallelModelTotals {
        ParallelModelTotals {
            attempts: self.attempts.load(Ordering::Acquire),
            input_tokens: self.input_tokens.load(Ordering::Acquire),
            cached_input_tokens: self.cached_input_tokens.load(Ordering::Acquire),
            cache_write_input_tokens: self.cache_write_input_tokens.load(Ordering::Acquire),
            output_tokens: self.output_tokens.load(Ordering::Acquire),
            reasoning_output_tokens: self.reasoning_output_tokens.load(Ordering::Acquire),
            known_cost_micros: self.known_cost_micros.load(Ordering::Acquire),
            cost_observations: self.cost_observations.load(Ordering::Acquire),
        }
    }
}

async fn run_target(
    index: usize,
    target: ParallelModelTarget,
    port: Arc<dyn ModelPort>,
    tracker: Arc<BudgetTracker>,
    batch_started: Instant,
    mut cancel: watch::Receiver<bool>,
) -> IndexedResult {
    let started = Instant::now();
    let mut records = Vec::with_capacity(target.attempts.len());
    let mut last_status = ParallelModelStatus::Failed;
    for attempt in target.attempts {
        if *cancel.borrow() {
            last_status = ParallelModelStatus::Cancelled;
            break;
        }
        let remaining = tracker
            .budget
            .max_wall_time
            .map(|limit| limit.saturating_sub(batch_started.elapsed()));
        if remaining.is_some_and(|limit| limit.is_zero()) {
            last_status = ParallelModelStatus::TimedOut;
            break;
        }
        if !tracker.reserve_attempt() {
            last_status = ParallelModelStatus::BudgetExceeded;
            break;
        }
        let attempt_started = Instant::now();
        let route = attempt.route;
        let result = tokio::select! {
            biased;
            () = cancellation(&mut cancel) => Err((ParallelModelStatus::Cancelled,
                ModelPortFailure::new("CANCELLED", "parallel model batch cancelled"), true)),
            () = wait_for_limit(remaining) => Err((ParallelModelStatus::TimedOut,
                ModelPortFailure::new("BATCH_TIMEOUT", "parallel model batch timed out"), true)),
            () = wait_for_limit(tracker.budget.attempt_timeout) => Err((ParallelModelStatus::TimedOut,
                ModelPortFailure::new("TIMEOUT", "parallel model attempt timed out"), false)),
            result = execute(&*port, attempt.request) => result.map_err(|failure|
                (ParallelModelStatus::Failed, failure, false)),
        };
        match result {
            Ok(success) => {
                tracker.record(success.usage, success.cost_micros);
                records.push(ParallelModelAttemptRecord {
                    route: route.clone(),
                    latency: attempt_started.elapsed(),
                    usage: success.usage,
                    cost_micros: success.cost_micros,
                    failure: success.failure.clone(),
                });
                if success.failure.is_some() {
                    last_status = ParallelModelStatus::Failed;
                    continue;
                }
                return IndexedResult {
                    index,
                    result: ParallelModelResult {
                        target_id: target.target_id,
                        status: ParallelModelStatus::Succeeded,
                        selected_route: Some(route),
                        frames: success.frames,
                        attempts: records,
                        latency: started.elapsed(),
                    },
                };
            }
            Err((status, failure, stop)) => {
                last_status = status;
                records.push(ParallelModelAttemptRecord {
                    route,
                    latency: attempt_started.elapsed(),
                    usage: None,
                    cost_micros: None,
                    failure: Some(failure),
                });
                if stop {
                    break;
                }
            }
        }
    }

    IndexedResult {
        index,
        result: ParallelModelResult {
            target_id: target.target_id,
            status: last_status,
            selected_route: None,
            frames: Vec::new(),
            attempts: records,
            latency: started.elapsed(),
        },
    }
}

struct AttemptOutcome {
    frames: Vec<String>,
    usage: Option<ProviderTokenUsage>,
    cost_micros: Option<u64>,
    failure: Option<ModelPortFailure>,
}

async fn execute(
    port: &dyn ModelPort,
    request: ModelPortRequest,
) -> Result<AttemptOutcome, ModelPortFailure> {
    let mut stream = port.stream(request).await?;
    let mut frames = Vec::new();
    while let Some(frame) = stream.next().await {
        let frame = frame?;
        match terminal(&frame)? {
            Some(terminal) => {
                frames.push(frame);
                return Ok(AttemptOutcome {
                    frames,
                    usage: terminal.usage,
                    cost_micros: terminal.cost_micros,
                    failure: terminal.failure,
                });
            }
            None => frames.push(frame),
        }
    }
    Err(ModelPortFailure::new(
        "STREAM_CLOSED",
        "model stream ended without a terminal frame",
    ))
}

struct Terminal {
    usage: Option<ProviderTokenUsage>,
    cost_micros: Option<u64>,
    failure: Option<ModelPortFailure>,
}

fn terminal(frame: &str) -> Result<Option<Terminal>, ModelPortFailure> {
    let value: Value = serde_json::from_str(frame)
        .map_err(|_| ModelPortFailure::new("PROTOCOL", "model stream frame is invalid"))?;
    match value.get("type").and_then(Value::as_str) {
        Some("completed" | "error") => Ok(Some(Terminal {
            usage: value
                .get("tokenUsage")
                .or_else(|| value.get("token_usage"))
                .map(parse_usage)
                .transpose()?,
            cost_micros: optional_u64(&value, "actualCostMicros", "actual_cost_micros")?,
            failure: if value["type"] == "error" {
                let error = value
                    .get("error")
                    .ok_or_else(|| ModelPortFailure::new("PROTOCOL", "model error is invalid"))?;
                Some(ModelPortFailure {
                    code: required_string(error, "code")?,
                    message: required_string(error, "message")?,
                    status: optional_u64(error, "status", "status")?
                        .map(u16::try_from)
                        .transpose()
                        .map_err(|_| ModelPortFailure::new("PROTOCOL", "model error is invalid"))?,
                    provider_retry_after_millis: optional_u64(
                        error,
                        "providerRetryAfterMillis",
                        "provider_retry_after_millis",
                    )?,
                    provider_request_id: optional_string(
                        error,
                        "providerRequestId",
                        "provider_request_id",
                    )?,
                })
            } else {
                None
            },
        })),
        Some(_) => Ok(None),
        None => Err(ModelPortFailure::new(
            "PROTOCOL",
            "model stream frame is invalid",
        )),
    }
}

fn parse_usage(value: &Value) -> Result<ProviderTokenUsage, ModelPortFailure> {
    Ok(ProviderTokenUsage {
        input_tokens: required_u64(value, "inputTokens", "input_tokens")?,
        cached_input_tokens: optional_u64(value, "cachedInputTokens", "cached_input_tokens")?
            .unwrap_or(0),
        cache_write_input_tokens: optional_u64(
            value,
            "cacheWriteInputTokens",
            "cache_write_input_tokens",
        )?
        .unwrap_or(0),
        output_tokens: required_u64(value, "outputTokens", "output_tokens")?,
        reasoning_output_tokens: optional_u64(
            value,
            "reasoningOutputTokens",
            "reasoning_output_tokens",
        )?
        .unwrap_or(0),
    })
}

fn required_u64(value: &Value, camel: &str, snake: &str) -> Result<u64, ModelPortFailure> {
    optional_u64(value, camel, snake)?.ok_or_else(|| {
        ModelPortFailure::new("PROTOCOL", "model usage metric is missing or invalid")
    })
}

fn optional_u64(value: &Value, camel: &str, snake: &str) -> Result<Option<u64>, ModelPortFailure> {
    let Some(metric) = value.get(camel).or_else(|| value.get(snake)) else {
        return Ok(None);
    };
    metric
        .as_u64()
        .map(Some)
        .ok_or_else(|| ModelPortFailure::new("PROTOCOL", "model metric is invalid"))
}

fn required_string(value: &Value, field: &str) -> Result<String, ModelPortFailure> {
    value
        .get(field)
        .and_then(Value::as_str)
        .filter(|value| !value.is_empty())
        .map(ToOwned::to_owned)
        .ok_or_else(|| ModelPortFailure::new("PROTOCOL", "model error is invalid"))
}

fn optional_string(
    value: &Value,
    camel: &str,
    snake: &str,
) -> Result<Option<String>, ModelPortFailure> {
    let Some(text) = value.get(camel).or_else(|| value.get(snake)) else {
        return Ok(None);
    };
    text.as_str()
        .filter(|text| !text.is_empty())
        .map(|text| Some(text.to_owned()))
        .ok_or_else(|| ModelPortFailure::new("PROTOCOL", "model error is invalid"))
}

fn validate(
    targets: &[ParallelModelTarget],
    budget: ParallelModelBudget,
) -> Result<(), ParallelModelRunError> {
    if targets.is_empty()
        || budget.max_attempts == Some(0)
        || budget.max_total_tokens == Some(0)
        || budget.max_total_cost_micros == Some(0)
        || budget.max_wall_time.is_some_and(|limit| limit.is_zero())
        || budget.attempt_timeout.is_some_and(|limit| limit.is_zero())
    {
        return Err(ParallelModelRunError);
    }
    let mut identities = HashSet::with_capacity(targets.len());
    if targets.iter().any(|target| {
        target.target_id.is_empty()
            || !identities.insert(target.target_id.as_str())
            || target.attempts.is_empty()
            || target.attempts.iter().any(|attempt| {
                attempt.route.is_empty()
                    || attempt.request.request_id.is_empty()
                    || attempt.request.payload_json.is_empty()
            })
    }) {
        return Err(ParallelModelRunError);
    }
    Ok(())
}

async fn wait_for_limit(limit: Option<Duration>) {
    match limit {
        Some(duration) => tokio::time::sleep(duration).await,
        None => pending::<()>().await,
    }
}

async fn cancellation(receiver: &mut watch::Receiver<bool>) {
    if *receiver.borrow_and_update() {
        return;
    }
    while receiver.changed().await.is_ok() {
        if *receiver.borrow_and_update() {
            return;
        }
    }
    pending::<()>().await;
}

fn saturating_add(value: &AtomicU64, amount: u64) {
    let _ = value.fetch_update(Ordering::AcqRel, Ordering::Acquire, |current| {
        Some(current.saturating_add(amount))
    });
}

#[cfg(test)]
mod tests {
    use std::{
        pin::Pin,
        sync::Mutex,
        sync::atomic::{AtomicU64, Ordering},
        task::{Context, Poll},
    };

    use super::*;
    use futures::{Stream, future::BoxFuture, stream};
    use serde_json::Value as JsonValue;

    #[derive(Debug, Default)]
    struct FixtureState {
        active: AtomicU64,
        peak: AtomicU64,
        requests: Mutex<Vec<ModelPortRequest>>,
    }

    #[derive(Debug, Default)]
    struct FixturePort {
        state: Arc<FixtureState>,
    }

    struct ActiveStream {
        inner: Pin<Box<dyn Stream<Item = Result<String, ModelPortFailure>> + Send>>,
        state: Arc<FixtureState>,
    }

    impl Stream for ActiveStream {
        type Item = Result<String, ModelPortFailure>;

        fn poll_next(
            mut self: Pin<&mut Self>,
            context: &mut Context<'_>,
        ) -> Poll<Option<Self::Item>> {
            self.inner.as_mut().poll_next(context)
        }
    }

    impl Drop for ActiveStream {
        fn drop(&mut self) {
            self.state.active.fetch_sub(1, Ordering::SeqCst);
        }
    }

    impl ModelPort for FixturePort {
        fn stream(
            &self,
            request: ModelPortRequest,
        ) -> BoxFuture<'static, Result<winwincode_kernel::ModelPortStream, ModelPortFailure>>
        {
            self.state
                .requests
                .lock()
                .expect("request log")
                .push(ModelPortRequest {
                    request_id: request.request_id.clone(),
                    payload_json: request.payload_json.clone(),
                });
            let current = self.state.active.fetch_add(1, Ordering::SeqCst) + 1;
            self.state.peak.fetch_max(current, Ordering::SeqCst);
            let state = Arc::clone(&self.state);
            let request_id = request.request_id;
            Box::pin(async move {
                let inner: Pin<Box<dyn Stream<Item = Result<String, ModelPortFailure>> + Send>> =
                    match request_id.as_str() {
                        "measured-failure" => Box::pin(stream::iter([Ok(
                            r#"{"type":"error","error":{"code":"MAX_TOKENS","message":"output limit"},"tokenUsage":{"input_tokens":10,"cached_input_tokens":2,"cache_write_input_tokens":0,"output_tokens":4,"reasoning_output_tokens":1}}"#.into(),
                        )])),
                        id if id.ends_with(":fail") || id == "fail" => {
                            Box::pin(stream::iter([Err(ModelPortFailure::new(
                                "SERVER",
                                "fixture failure",
                            ))]))
                        }
                        id if id.ends_with(":hang") || id == "hang" => Box::pin(stream::pending()),
                        _ => Box::pin(stream::once(async move {
                            tokio::time::sleep(Duration::from_millis(20)).await;
                            Ok(format!(
                                r#"{{"type":"completed","responseId":"{request_id}","tokenUsage":{{"input_tokens":3,"cached_input_tokens":1,"cache_write_input_tokens":0,"output_tokens":2,"reasoning_output_tokens":1}},"actualCostMicros":7}}"#
                            ))
                        })),
                    };
                Ok(Box::pin(ActiveStream { inner, state }) as winwincode_kernel::ModelPortStream)
            })
        }
    }

    fn attempt(route: &str, request_id: &str) -> ParallelModelAttempt {
        ParallelModelAttempt {
            route: route.to_owned(),
            request: ModelPortRequest {
                request_id: request_id.to_owned(),
                payload_json: r#"{"model":"fixture","reasoning":{"effort":"high"}}"#.to_owned(),
            },
        }
    }

    fn dynasty_payload(route: &str, effort: &str) -> String {
        serde_json::json!({
            "model": route,
            "reasoning": {"effort": effort},
        })
        .to_string()
    }

    fn budget(max_attempts: u64) -> ParallelModelBudget {
        ParallelModelBudget {
            max_attempts: Some(max_attempts),
            max_total_tokens: Some(100),
            max_total_cost_micros: Some(100),
            max_wall_time: Some(Duration::from_secs(1)),
            attempt_timeout: Some(Duration::from_millis(40)),
        }
    }

    #[tokio::test]
    async fn failed_attempt_keeps_measured_usage_before_fallback() {
        let runner = ParallelModelRunner::new(Arc::new(FixturePort::default()));
        let (_cancel, signal) = parallel_model_cancellation();
        let batch = runner
            .run(
                vec![ParallelModelTarget {
                    target_id: "measured".into(),
                    attempts: vec![
                        attempt("first", "measured-failure"),
                        attempt("second", "ok"),
                    ],
                }],
                ParallelModelBudget::default(),
                signal,
            )
            .await
            .unwrap();
        let result = &batch.results[0];
        assert_eq!(result.status, ParallelModelStatus::Succeeded);
        assert_eq!(result.selected_route.as_deref(), Some("second"));
        assert_eq!(
            result.attempts[0].failure.as_ref().unwrap().code,
            "MAX_TOKENS"
        );
        assert_eq!(result.attempts[0].usage.unwrap().input_tokens, 10);
        assert_eq!(result.attempts[0].cost_micros, None);
        assert_eq!(batch.totals.input_tokens, 13);
        assert_eq!(batch.totals.output_tokens, 6);
    }

    #[tokio::test]
    async fn parallel_runner_keeps_partial_success_fallback_budget_and_cancel_bounded() {
        let port = Arc::new(FixturePort::default());
        let runner = ParallelModelRunner::new(port.clone());
        let (_cancel, signal) = parallel_model_cancellation();
        let batch = runner
            .run(
                vec![
                    ParallelModelTarget {
                        target_id: "direct".to_owned(),
                        attempts: vec![attempt("provider-a/model-a", "direct")],
                    },
                    ParallelModelTarget {
                        target_id: "fallback".to_owned(),
                        attempts: vec![
                            attempt("provider-b/model-b", "fail"),
                            attempt("provider-c/model-c", "fallback"),
                        ],
                    },
                    ParallelModelTarget {
                        target_id: "timeout".to_owned(),
                        attempts: vec![attempt("provider-d/model-d", "hang")],
                    },
                ],
                budget(4),
                signal,
            )
            .await
            .expect("valid batch");
        assert_eq!(
            batch
                .results
                .iter()
                .map(|result| result.status)
                .collect::<Vec<_>>(),
            vec![
                ParallelModelStatus::Succeeded,
                ParallelModelStatus::Succeeded,
                ParallelModelStatus::TimedOut,
            ]
        );
        assert_eq!(
            batch.results[1].selected_route.as_deref(),
            Some("provider-c/model-c")
        );
        assert_eq!(batch.totals.known_cost_micros, 14);
        assert!(port.state.peak.load(Ordering::SeqCst) >= 2);

        let (cancel, signal) = parallel_model_cancellation();
        cancel.cancel();
        let cancelled = runner
            .run(
                vec![ParallelModelTarget {
                    target_id: "cancelled".to_owned(),
                    attempts: vec![attempt("provider/model", "hang")],
                }],
                budget(1),
                signal,
            )
            .await
            .expect("valid cancelled batch");
        assert_eq!(cancelled.results[0].status, ParallelModelStatus::Cancelled);

        let (_cancel, signal) = parallel_model_cancellation();
        let limited = runner
            .run(
                vec![ParallelModelTarget {
                    target_id: "budget".to_owned(),
                    attempts: vec![attempt("one", "fail"), attempt("two", "fallback")],
                }],
                budget(1),
                signal,
            )
            .await
            .expect("valid limited batch");
        assert_eq!(
            limited.results[0].status,
            ParallelModelStatus::BudgetExceeded
        );
    }

    #[tokio::test]
    async fn unlimited_batch_records_usage_and_remains_cancellable() {
        let port = Arc::new(FixturePort::default());
        let runner = ParallelModelRunner::new(port.clone());
        let (_cancel, signal) = parallel_model_cancellation();
        let batch = runner
            .run(
                vec![ParallelModelTarget {
                    target_id: "unlimited".into(),
                    attempts: vec![attempt("first", "fail"), attempt("second", "ok")],
                }],
                ParallelModelBudget::default(),
                signal,
            )
            .await
            .unwrap();
        assert_eq!(batch.results[0].status, ParallelModelStatus::Succeeded);
        assert_eq!(batch.results[0].attempts.len(), 2);
        assert!(batch.results[0].attempts[0].failure.is_some());
        assert_eq!(batch.totals.attempts, 2);
        assert_eq!(batch.totals.input_tokens, 3);
        assert_eq!(batch.totals.output_tokens, 2);
        assert_eq!(batch.totals.known_cost_micros, 7);
        let tracker = BudgetTracker::new(ParallelModelBudget::default());
        tracker.record(None, Some(u64::MAX));
        assert!(tracker.reserve_attempt());
        let (cancel, signal) = parallel_model_cancellation();
        let task = runner.run(
            vec![ParallelModelTarget {
                target_id: "cancel-unlimited".into(),
                attempts: vec![attempt("hanging", "hang")],
            }],
            ParallelModelBudget::default(),
            signal,
        );
        let cancel_after_open = async {
            while port.state.active.load(Ordering::SeqCst) == 0 {
                tokio::task::yield_now().await;
            }
            cancel.cancel();
        };
        let (cancelled, ()) = tokio::time::timeout(Duration::from_secs(1), async {
            tokio::join!(task, cancel_after_open)
        })
        .await
        .expect("unlimited cancellation must finish");
        assert_eq!(
            cancelled.unwrap().results[0].status,
            ParallelModelStatus::Cancelled
        );
        assert_eq!(port.state.active.load(Ordering::SeqCst), 0);
    }

    #[tokio::test]
    async fn batch_stop_preserves_prior_failure_and_interrupted_attempt() {
        for timed_out in [false, true] {
            let port = Arc::new(FixturePort::default());
            let runner = ParallelModelRunner::new(port.clone());
            let (cancel, signal) = parallel_model_cancellation();
            let task = runner.run(
                vec![ParallelModelTarget {
                    target_id: "interrupted".into(),
                    attempts: vec![
                        attempt("first", "fail"),
                        attempt("second", "hang"),
                        attempt("never-opened", "ok"),
                    ],
                }],
                ParallelModelBudget {
                    max_wall_time: timed_out.then_some(Duration::from_millis(40)),
                    ..ParallelModelBudget::default()
                },
                signal,
            );
            let stop = async {
                if !timed_out {
                    while port.state.requests.lock().unwrap().len() < 2 {
                        tokio::task::yield_now().await;
                    }
                    cancel.cancel();
                }
            };
            let (batch, ()) =
                tokio::time::timeout(Duration::from_secs(1), async { tokio::join!(task, stop) })
                    .await
                    .unwrap();
            let batch = batch.unwrap();
            let result = &batch.results[0];
            assert_eq!(
                result.status,
                if timed_out {
                    ParallelModelStatus::TimedOut
                } else {
                    ParallelModelStatus::Cancelled
                }
            );
            assert_eq!(result.attempts.len(), 2);
            assert_eq!(result.attempts[0].failure.as_ref().unwrap().code, "SERVER");
            assert_eq!(
                result.attempts[1].failure.as_ref().unwrap().code,
                if timed_out {
                    "BATCH_TIMEOUT"
                } else {
                    "CANCELLED"
                }
            );
            assert!(result.attempts[1].usage.is_none());
            assert!(result.attempts[1].cost_micros.is_none());
            assert_eq!(batch.totals.attempts, 2);
            assert_eq!(port.state.requests.lock().unwrap().len(), 2);
            assert_eq!(port.state.active.load(Ordering::SeqCst), 0);
        }
    }

    /// Seven dynasty seats; FABLE fails and QWEN hangs so partial success,
    /// reasoning-effort passthrough, cost and latency can all be asserted.
    fn dynasty_panel_seats() -> Vec<FusionPanelSeat> {
        let mut seats = vec![
            FusionPanelSeat {
                model: fusion_panel::ASTRA.to_owned(),
                routes: vec!["runtime/astra".to_owned()],
            },
            FusionPanelSeat {
                model: fusion_panel::SOL.to_owned(),
                routes: vec![
                    "runtime/sol-primary".to_owned(),
                    "runtime/sol-fallback".to_owned(),
                ],
            },
            FusionPanelSeat {
                model: fusion_panel::FABLE.to_owned(),
                routes: vec!["runtime/fable".to_owned()],
            },
            FusionPanelSeat {
                model: fusion_panel::OPUS.to_owned(),
                routes: vec!["runtime/opus".to_owned()],
            },
            FusionPanelSeat {
                model: fusion_panel::GLM.to_owned(),
                routes: vec!["runtime/glm".to_owned()],
            },
            FusionPanelSeat {
                model: fusion_panel::KIMI.to_owned(),
                routes: vec!["runtime/kimi".to_owned()],
            },
            FusionPanelSeat {
                model: fusion_panel::QWEN.to_owned(),
                routes: vec!["runtime/qwen".to_owned()],
            },
        ];
        for seat in &mut seats {
            match seat.model.as_str() {
                fusion_panel::FABLE => seat.routes[0] = "runtime/fable-fail".to_owned(),
                fusion_panel::QWEN => seat.routes[0] = "runtime/qwen-hang".to_owned(),
                _ => {}
            }
        }
        seats
    }

    #[tokio::test]
    async fn dynasty_panel_preserves_partial_success_reasoning_cost_and_latency() {
        let port = Arc::new(FixturePort::default());
        let runner = ParallelModelRunner::new(port.clone());
        let seats = dynasty_panel_seats();
        let targets = seats
            .iter()
            .map(|seat| {
                let mut target = seat.to_target("fusion", |route| {
                    let effort = if route.contains("opus") {
                        "high"
                    } else {
                        "medium"
                    };
                    dynasty_payload(route, effort)
                });
                for attempt in &mut target.attempts {
                    // SOL primary and FABLE both force a terminal failure.
                    if (seat.model == fusion_panel::SOL && attempt.route.ends_with("-primary"))
                        || seat.model == fusion_panel::FABLE
                    {
                        attempt.request.request_id = format!("fusion:{}:fail", seat.model);
                    } else if seat.model == fusion_panel::QWEN {
                        attempt.request.request_id = format!("fusion:{}:hang", seat.model);
                    } else if attempt.route.ends_with("-fallback") {
                        attempt.request.request_id = format!("fusion:{}:ok", seat.model);
                    }
                }
                target
            })
            .collect::<Vec<_>>();
        let (_cancel, signal) = parallel_model_cancellation();
        let batch = runner
            .run(targets, budget(16), signal)
            .await
            .expect("valid dynasty batch");
        assert_eq!(batch.results.len(), 7);
        let by_id = |id: &str| {
            batch
                .results
                .iter()
                .find(|result| result.target_id == id)
                .expect("target present")
        };
        assert_eq!(
            by_id(fusion_panel::ASTRA).status,
            ParallelModelStatus::Succeeded
        );
        assert_eq!(
            by_id(fusion_panel::SOL).selected_route.as_deref(),
            Some("runtime/sol-fallback")
        );
        assert_eq!(
            by_id(fusion_panel::FABLE).status,
            ParallelModelStatus::Failed
        );
        assert_eq!(
            by_id(fusion_panel::QWEN).status,
            ParallelModelStatus::TimedOut
        );
        let succeeded = batch
            .results
            .iter()
            .filter(|result| result.status == ParallelModelStatus::Succeeded)
            .count();
        assert_eq!(succeeded, 5);
        assert_eq!(
            batch.totals.known_cost_micros,
            7 * u64::try_from(succeeded).expect("count")
        );
        assert!(by_id(fusion_panel::OPUS).latency > Duration::ZERO);
        assert!(batch.latency > Duration::ZERO);
        let requests = port.state.requests.lock().expect("request log").clone();
        assert!(requests.iter().any(|request| {
            request.request_id.contains("opus")
                && request.payload_json.contains(r#""effort":"high""#)
        }));
        assert!(requests.iter().any(|request| {
            request.payload_json.contains("runtime/glm")
                && request.payload_json.contains(r#""effort":"medium""#)
        }));
        for request in &requests {
            let payload: JsonValue = serde_json::from_str(&request.payload_json).expect("payload");
            assert!(payload.get("apiKey").is_none());
            assert!(payload.get("secret").is_none());
        }

        // Control-plane compose seam feed: only succeeded targets, target_id
        // equals Fusion candidate_id, frames preserved for answer extraction.
        let rows = fusion_compose_frame_rows(&batch);
        assert_eq!(rows.len(), 5);
        assert!(rows.iter().any(|(id, _)| id == fusion_panel::ASTRA));
        assert!(rows.iter().any(|(id, _)| id == fusion_panel::SOL));
        assert!(rows.iter().all(|(id, _)| *id != fusion_panel::FABLE));
        assert!(rows.iter().all(|(id, _)| *id != fusion_panel::QWEN));
        assert!(rows.iter().all(|(_, frames)| !frames.is_empty()));
    }

    #[tokio::test]
    async fn invalid_parallel_runner_inputs_fail_before_provider_open() {
        let port = Arc::new(FixturePort::default());
        let runner = ParallelModelRunner::new(port.clone());
        let (_cancel, signal) = parallel_model_cancellation();
        let error = runner
            .run(Vec::new(), budget(1), signal)
            .await
            .expect_err("empty targets are invalid");
        assert_eq!(error, ParallelModelRunError);
        let (_cancel, signal) = parallel_model_cancellation();
        let duplicate = runner
            .run(
                vec![
                    ParallelModelTarget {
                        target_id: fusion_panel::GLM.to_owned(),
                        attempts: vec![attempt("runtime/glm", "ok")],
                    },
                    ParallelModelTarget {
                        target_id: fusion_panel::GLM.to_owned(),
                        attempts: vec![attempt("runtime/glm-2", "ok")],
                    },
                ],
                budget(2),
                signal,
            )
            .await
            .expect_err("duplicate target ids are invalid");
        assert_eq!(duplicate, ParallelModelRunError);
        assert!(port.state.requests.lock().expect("request log").is_empty());
    }

    #[test]
    fn default_fusion_panel_lists_model_dynasty_identities() {
        assert_eq!(
            fusion_panel::default_panel(),
            [
                fusion_panel::ASTRA,
                fusion_panel::SOL,
                fusion_panel::FABLE,
                fusion_panel::OPUS,
                fusion_panel::GLM,
                fusion_panel::KIMI,
                fusion_panel::QWEN,
            ]
        );
    }
}
