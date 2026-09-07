// SPDX-License-Identifier: Apache-2.0

//! Bounded, secret-safe operational telemetry for `WinWinCode` processes.
//!
//! This crate accepts only closed metric dimensions, stable correlation
//! identities, and references to facts that their owning subsystem already
//! validated. It never accepts prompts, request bodies, credential material,
//! command arguments, file contents, or arbitrary log messages. Telemetry is
//! diagnostic state only and never becomes a business authority.

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::{collections::BTreeSet, fmt};

const MAX_SAFE_INTEGER: u64 = 9_007_199_254_740_991;
const MAX_SOURCE_ID_BYTES: usize = 160;
const MAX_RULES: usize = 64;
const MAX_CONFIGURED_ROWS: u64 = 1_000_000;
const MAX_QUERY_ROWS: u32 = 200;
const MAX_QUERY_BUCKETS: u32 = 1_440;
const MIN_BUCKET_WIDTH_MILLIS: u64 = 1_000;
const MAX_BUCKET_WIDTH_MILLIS: u64 = 86_400_000;

/// Stable identity for one submitted observation.
#[derive(Clone, Debug, Eq, Hash, PartialEq, Serialize, Deserialize)]
pub struct ObservationId(String);

impl ObservationId {
    /// Creates a canonical `obs_` identity.
    ///
    /// # Errors
    ///
    /// Rejects identities outside the canonical Crockford format.
    pub fn try_new(value: impl Into<String>) -> Result<Self, ObservabilityError> {
        let value = value.into();
        if !canonical_id(&value, "obs") {
            return Err(ObservabilityError::invalid());
        }
        Ok(Self(value))
    }

    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

/// W3C-compatible lower-case trace identity.
#[derive(Clone, Debug, Eq, Hash, PartialEq, Serialize, Deserialize)]
pub struct TraceId(String);

impl TraceId {
    /// Creates a 128-bit lower-case hexadecimal trace identity.
    ///
    /// # Errors
    ///
    /// Rejects malformed or all-zero identities.
    pub fn try_new(value: impl Into<String>) -> Result<Self, ObservabilityError> {
        let value = value.into();
        if !lower_hex(&value, 32) || value.bytes().all(|byte| byte == b'0') {
            return Err(ObservabilityError::invalid());
        }
        Ok(Self(value))
    }

    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

/// W3C-compatible lower-case span identity.
#[derive(Clone, Debug, Eq, Hash, PartialEq, Serialize, Deserialize)]
pub struct SpanId(String);

impl SpanId {
    /// Creates a 64-bit lower-case hexadecimal span identity.
    ///
    /// # Errors
    ///
    /// Rejects malformed or all-zero identities.
    pub fn try_new(value: impl Into<String>) -> Result<Self, ObservabilityError> {
        let value = value.into();
        if !lower_hex(&value, 16) || value.bytes().all(|byte| byte == b'0') {
            return Err(ObservabilityError::invalid());
        }
        Ok(Self(value))
    }

    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

/// Canonical digest of a source fact or correlation authority.
#[derive(Clone, Debug, Eq, Hash, PartialEq, Serialize, Deserialize)]
pub struct FactDigest(String);

impl FactDigest {
    /// Creates a canonical lower-case SHA-256 digest.
    ///
    /// # Errors
    ///
    /// Rejects malformed digests.
    pub fn try_new(value: impl Into<String>) -> Result<Self, ObservabilityError> {
        let value = value.into();
        if !sha256_digest(&value) {
            return Err(ObservabilityError::invalid());
        }
        Ok(Self(value))
    }

    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

/// Stable, bounded identity of an already-validated source fact.
#[derive(Clone, Debug, Eq, Hash, PartialEq, Serialize, Deserialize)]
pub struct SourceFactId(String);

impl SourceFactId {
    /// Creates a source identity used only for replay deduplication.
    ///
    /// # Errors
    ///
    /// Rejects blank, oversized, control-bearing, credential-shaped, or
    /// non-portable values.
    pub fn try_new(value: impl Into<String>) -> Result<Self, ObservabilityError> {
        let value = value.into();
        let normalized = value.to_ascii_lowercase();
        let safe = !value.is_empty()
            && value.len() <= MAX_SOURCE_ID_BYTES
            && value.bytes().all(|byte| {
                byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'-' | b'.' | b':' | b'/')
            })
            && !secret_markers()
                .iter()
                .any(|marker| normalized.contains(marker));
        if !safe {
            return Err(ObservabilityError::invalid());
        }
        Ok(Self(value))
    }

    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

/// Closed source authority for a secret-safe observation.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ObservationSourceKind {
    RuntimeEvent,
    UsageLedger,
    AuditLedger,
    InternalOperation,
}

/// Reference to a fact owned and validated outside observability.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ObservationSource {
    pub kind: ObservationSourceKind,
    pub fact_id: SourceFactId,
    pub fact_digest: FactDigest,
}

/// Closed process component. This is safe to use as a metric label.
#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Component {
    Http,
    WebSocket,
    Scheduler,
    Worker,
    Provider,
    Storage,
    Queue,
}

/// Closed operation category. No route, model, tenant, or user value can enter
/// the metric label set.
#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Operation {
    HttpRequest,
    WebSocketConnect,
    WebSocketPublish,
    SchedulerTick,
    SchedulerDispatch,
    WorkerHeartbeat,
    WorkerLease,
    WorkerRecovery,
    ProviderOpen,
    ProviderStream,
    ProviderSettlement,
    StorageRead,
    StorageWrite,
    QueueEnqueue,
    QueueDequeue,
    QueueRetry,
}

/// Closed result class. Raw error text and status codes are excluded.
#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Outcome {
    Succeeded,
    ClientError,
    ServerError,
    Timeout,
    Cancelled,
    Fenced,
    Saturated,
    Recovered,
    Failed,
}

/// Closed capacity resource label.
#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CapacityResource {
    HttpInFlight,
    WebSocketConnections,
    SchedulerReadyJobs,
    WorkerAvailableSlots,
    ProviderInFlight,
    StorageBusyWriters,
    QueueDepth,
}

/// Closed structured-log severity.
#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LogSeverity {
    Info,
    Warning,
    Error,
}

/// Closed diagnostic code. Arbitrary messages cannot enter structured logs.
#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DiagnosticCode {
    RequestRejected,
    ConnectionClosed,
    SchedulerStalled,
    WorkerHeartbeatMissed,
    ProviderRateLimited,
    StorageBusy,
    QueueBacklog,
    RecoveryStarted,
    RecoveryCompleted,
    RecoveryFailed,
}

/// Stable trace context shared across structured metrics, logs, and alerts.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TraceContext {
    pub trace_id: TraceId,
    pub span_id: SpanId,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub parent_span_id: Option<SpanId>,
}

impl TraceContext {
    /// Deterministically derives a trace and span from a pre-validated
    /// correlation digest and closed span dimensions.
    ///
    /// `span_ordinal` distinguishes repeated spans of the same operation. It
    /// must come from the caller's durable sequence, never from current time.
    ///
    /// # Errors
    ///
    /// Returns an error only if an internal derived identity is malformed.
    pub fn derive(
        correlation: &FactDigest,
        component: Component,
        operation: Operation,
        span_ordinal: u64,
        parent_span_id: Option<SpanId>,
    ) -> Result<Self, ObservabilityError> {
        if span_ordinal == 0 || span_ordinal > MAX_SAFE_INTEGER {
            return Err(ObservabilityError::invalid());
        }
        let trace_hex = correlation
            .as_str()
            .strip_prefix("sha256:")
            .ok_or_else(ObservabilityError::invalid)?;
        let trace_id = TraceId::try_new(trace_hex[..32].to_owned())?;
        let span_material = format!(
            "winwincode-observability-span-v1\0{}\0{component:?}\0{operation:?}\0{span_ordinal}",
            correlation.as_str()
        );
        let span_hex = format!("{:x}", Sha256::digest(span_material.as_bytes()));
        let span_id = SpanId::try_new(span_hex[..16].to_owned())?;
        if parent_span_id.as_ref() == Some(&span_id) {
            return Err(ObservabilityError::invalid());
        }
        Ok(Self {
            trace_id,
            span_id,
            parent_span_id,
        })
    }

    /// Validates canonical trace and parent-span identities.
    ///
    /// # Errors
    ///
    /// Rejects malformed identities and self-parenting spans.
    pub fn validate(&self) -> Result<(), ObservabilityError> {
        TraceId::try_new(self.trace_id.0.clone())?;
        SpanId::try_new(self.span_id.0.clone())?;
        if let Some(parent) = &self.parent_span_id {
            SpanId::try_new(parent.0.clone())?;
            if parent == &self.span_id {
                return Err(ObservabilityError::invalid());
            }
        }
        Ok(())
    }
}

/// One secret-safe signal.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum ObservationSignal {
    OperationCompleted {
        outcome: Outcome,
        latency_millis: u64,
    },
    CapacityObserved {
        resource: CapacityResource,
        used: u64,
        limit: u64,
    },
    RecoveryObserved {
        outcome: Outcome,
        latency_millis: u64,
        recovered_items: u64,
    },
    StructuredLog {
        severity: LogSeverity,
        code: DiagnosticCode,
    },
}

/// Complete structured observation. No field can contain a raw diagnostic,
/// input, credential, provider payload, path, or command.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Observation {
    pub observation_id: ObservationId,
    pub source: ObservationSource,
    pub trace: TraceContext,
    pub component: Component,
    pub operation: Operation,
    pub occurred_at_unix_millis: u64,
    pub signal: ObservationSignal,
}

impl Observation {
    /// Validates closed dimensions, bounded values, and safe identities.
    ///
    /// # Errors
    ///
    /// Rejects malformed or cross-component observations.
    pub fn validate(&self) -> Result<(), ObservabilityError> {
        ObservationId::try_new(self.observation_id.0.clone())?;
        SourceFactId::try_new(self.source.fact_id.0.clone())?;
        FactDigest::try_new(self.source.fact_digest.0.clone())?;
        self.trace.validate()?;
        safe_integer(self.occurred_at_unix_millis)?;
        if !operation_belongs_to(self.component, self.operation) {
            return Err(ObservabilityError::invalid());
        }
        match self.signal {
            ObservationSignal::OperationCompleted { latency_millis, .. } => {
                safe_integer(latency_millis)?;
            }
            ObservationSignal::CapacityObserved {
                resource,
                used,
                limit,
            } => {
                safe_integer(used)?;
                safe_integer(limit)?;
                if limit == 0 || !resource_belongs_to(self.component, resource) {
                    return Err(ObservabilityError::invalid());
                }
            }
            ObservationSignal::RecoveryObserved {
                latency_millis,
                recovered_items,
                outcome,
            } => {
                safe_integer(latency_millis)?;
                safe_integer(recovered_items)?;
                if !matches!(
                    self.operation,
                    Operation::WorkerRecovery | Operation::QueueRetry
                ) || !matches!(outcome, Outcome::Recovered | Outcome::Failed)
                {
                    return Err(ObservabilityError::invalid());
                }
            }
            ObservationSignal::StructuredLog { code, .. } => {
                if !diagnostic_belongs_to(self.component, code) {
                    return Err(ObservabilityError::invalid());
                }
            }
        }
        Ok(())
    }

    /// Checks accumulator values against configured durable row bounds.
    ///
    /// # Errors
    ///
    /// Rejects values that could overflow a configured aggregate.
    pub fn validate_for_config(
        &self,
        config: &ObservabilityConfig,
    ) -> Result<(), ObservabilityError> {
        let maximum_accumulator_increment = MAX_SAFE_INTEGER / config.max_receipts;
        match self.signal {
            ObservationSignal::OperationCompleted { latency_millis, .. } => {
                if latency_millis > maximum_accumulator_increment {
                    return Err(ObservabilityError::invalid());
                }
            }
            ObservationSignal::RecoveryObserved {
                latency_millis,
                recovered_items,
                ..
            } => {
                if latency_millis > maximum_accumulator_increment
                    || recovered_items > maximum_accumulator_increment
                {
                    return Err(ObservabilityError::invalid());
                }
            }
            ObservationSignal::CapacityObserved { .. }
            | ObservationSignal::StructuredLog { .. } => {}
        }
        Ok(())
    }
}

/// Stable configured alert rule identity.
#[derive(Clone, Debug, Eq, Hash, Ord, PartialEq, PartialOrd, Serialize, Deserialize)]
pub struct AlertRuleId(String);

impl AlertRuleId {
    /// Creates a low-cardinality configuration identity.
    ///
    /// # Errors
    ///
    /// Rejects blank, oversized, or non-portable identifiers.
    pub fn try_new(value: impl Into<String>) -> Result<Self, ObservabilityError> {
        let value = value.into();
        if value.is_empty()
            || value.len() > 64
            || !value.bytes().all(|byte| {
                byte.is_ascii_lowercase()
                    || byte.is_ascii_digit()
                    || matches!(byte, b'_' | b'-' | b'.')
            })
        {
            return Err(ObservabilityError::invalid());
        }
        Ok(Self(value))
    }

    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

/// Closed alert condition evaluated only against a matching structured signal.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum AlertCondition {
    LatencyAtLeast {
        component: Component,
        operation: Operation,
        threshold_millis: u64,
    },
    OutcomeEquals {
        component: Component,
        operation: Operation,
        outcome: Outcome,
    },
    CapacityRatioAtLeast {
        component: Component,
        resource: CapacityResource,
        numerator: u32,
        denominator: u32,
    },
    RecoveryFailed {
        component: Component,
        operation: Operation,
    },
}

impl AlertCondition {
    fn validate(&self) -> Result<(), ObservabilityError> {
        match *self {
            Self::LatencyAtLeast {
                component,
                operation,
                threshold_millis,
            } => {
                if threshold_millis == 0 || !operation_belongs_to(component, operation) {
                    return Err(ObservabilityError::invalid());
                }
                safe_integer(threshold_millis)?;
            }
            Self::OutcomeEquals {
                component,
                operation,
                ..
            } => {
                if !operation_belongs_to(component, operation) {
                    return Err(ObservabilityError::invalid());
                }
            }
            Self::CapacityRatioAtLeast {
                component,
                resource,
                numerator,
                denominator,
            } => {
                if numerator == 0
                    || denominator == 0
                    || numerator > denominator
                    || !resource_belongs_to(component, resource)
                {
                    return Err(ObservabilityError::invalid());
                }
            }
            Self::RecoveryFailed {
                component,
                operation,
            } => {
                if !operation_belongs_to(component, operation)
                    || !matches!(operation, Operation::WorkerRecovery | Operation::QueueRetry)
                {
                    return Err(ObservabilityError::invalid());
                }
            }
        }
        Ok(())
    }

    fn evaluate(&self, observation: &Observation) -> Option<bool> {
        match (self, &observation.signal) {
            (
                Self::LatencyAtLeast {
                    component,
                    operation,
                    threshold_millis,
                },
                ObservationSignal::OperationCompleted { latency_millis, .. },
            ) if observation.component == *component && observation.operation == *operation => {
                Some(latency_millis >= threshold_millis)
            }
            (
                Self::OutcomeEquals {
                    component,
                    operation,
                    outcome,
                },
                ObservationSignal::OperationCompleted {
                    outcome: observed, ..
                },
            ) if observation.component == *component && observation.operation == *operation => {
                Some(observed == outcome)
            }
            (
                Self::CapacityRatioAtLeast {
                    component,
                    resource,
                    numerator,
                    denominator,
                },
                ObservationSignal::CapacityObserved {
                    resource: observed,
                    used,
                    limit,
                },
            ) if observation.component == *component && observed == resource => Some(
                u128::from(*used) * u128::from(*denominator)
                    >= u128::from(*limit) * u128::from(*numerator),
            ),
            (
                Self::RecoveryFailed {
                    component,
                    operation,
                },
                ObservationSignal::RecoveryObserved { outcome, .. },
            ) if observation.component == *component && observation.operation == *operation => {
                Some(*outcome == Outcome::Failed)
            }
            _ => None,
        }
    }
}

/// Configured alert severity.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AlertSeverity {
    Warning,
    Critical,
}

/// One low-cardinality alert rule.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AlertRule {
    pub rule_id: AlertRuleId,
    pub severity: AlertSeverity,
    pub condition: AlertCondition,
}

/// Durable alert status.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AlertStatus {
    Firing,
    Resolved,
}

/// One durable, deduplicated alert transition.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AlertTransition {
    pub sequence: u64,
    pub alert_id: String,
    pub rule_id: AlertRuleId,
    pub severity: AlertSeverity,
    pub status: AlertStatus,
    pub generation: u64,
    pub trace: TraceContext,
    pub observation_id: ObservationId,
    pub occurred_at_unix_millis: u64,
}

/// Bounded durable-store and query policy.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ObservabilityConfig {
    pub bucket_width_millis: u64,
    pub max_receipts: u64,
    pub max_trace_rows: u64,
    pub max_metric_rows: u64,
    pub max_query_rows: u32,
    pub max_query_buckets: u32,
    pub alert_rules: Vec<AlertRule>,
}

impl ObservabilityConfig {
    /// Validates bounds and sorts alert rules into canonical order.
    ///
    /// # Errors
    ///
    /// Rejects invalid bounds, rules, or duplicate rule identities.
    pub fn validate_and_normalize(&mut self) -> Result<(), ObservabilityError> {
        if !(MIN_BUCKET_WIDTH_MILLIS..=MAX_BUCKET_WIDTH_MILLIS).contains(&self.bucket_width_millis)
            || !(1..=MAX_CONFIGURED_ROWS).contains(&self.max_receipts)
            || !(1..=MAX_CONFIGURED_ROWS).contains(&self.max_trace_rows)
            || !(1..=MAX_CONFIGURED_ROWS).contains(&self.max_metric_rows)
            || !(1..=MAX_QUERY_ROWS).contains(&self.max_query_rows)
            || !(1..=MAX_QUERY_BUCKETS).contains(&self.max_query_buckets)
            || self.alert_rules.len() > MAX_RULES
        {
            return Err(ObservabilityError::invalid());
        }
        self.alert_rules
            .sort_by(|left, right| left.rule_id.cmp(&right.rule_id));
        let mut rule_ids = BTreeSet::new();
        for rule in &self.alert_rules {
            AlertRuleId::try_new(rule.rule_id.0.clone())?;
            rule.condition.validate()?;
            if !rule_ids.insert(rule.rule_id.clone()) {
                return Err(ObservabilityError::invalid());
            }
        }
        Ok(())
    }
}

/// Result of recording one observation.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ObservationReceipt {
    pub accepted_sequence: u64,
    pub duplicate: bool,
    pub alert_transitions: Vec<AlertTransition>,
}

/// One retained trace or structured-log row.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct TraceRow {
    pub sequence: u64,
    pub observation: Observation,
}

/// Bounded trace page.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct TracePage {
    pub rows: Vec<TraceRow>,
    pub next_after_sequence: Option<u64>,
}

/// Closed metric-series identity.
#[derive(Clone, Debug, Eq, Ord, PartialEq, PartialOrd, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum MetricSeriesKey {
    Operation {
        component: Component,
        operation: Operation,
        outcome: Outcome,
    },
    Capacity {
        component: Component,
        resource: CapacityResource,
    },
    Recovery {
        component: Component,
        operation: Operation,
        outcome: Outcome,
    },
    StructuredLog {
        component: Component,
        severity: LogSeverity,
        code: DiagnosticCode,
    },
}

impl MetricSeriesKey {
    /// Validates that the closed dimensions form a supported series.
    ///
    /// # Errors
    ///
    /// Rejects cross-component operations, resources, and diagnostics.
    pub fn validate(&self) -> Result<(), ObservabilityError> {
        let valid = match *self {
            Self::Operation {
                component,
                operation,
                ..
            } => operation_belongs_to(component, operation),
            Self::Capacity {
                component,
                resource,
            } => resource_belongs_to(component, resource),
            Self::Recovery {
                component,
                operation,
                outcome,
            } => {
                operation_belongs_to(component, operation)
                    && matches!(operation, Operation::WorkerRecovery | Operation::QueueRetry)
                    && matches!(outcome, Outcome::Recovered | Outcome::Failed)
            }
            Self::StructuredLog {
                component, code, ..
            } => diagnostic_belongs_to(component, code),
        };
        if valid {
            Ok(())
        } else {
            Err(ObservabilityError::invalid())
        }
    }
}

/// One metric-series bucket. Fields not used by a series kind remain zero.
#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct MetricRow {
    pub bucket_start_unix_millis: u64,
    pub key: MetricSeriesKey,
    pub observations: u64,
    pub latency_total_millis: u64,
    pub latency_max_millis: u64,
    pub recovered_items: u64,
    pub latest_used: u64,
    pub latest_limit: u64,
    pub maximum_used: u64,
}

/// Cursor for deterministic metric ordering.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct MetricCursor {
    pub bucket_start_unix_millis: u64,
    pub key: MetricSeriesKey,
}

/// One bounded metric page.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct MetricPage {
    pub rows: Vec<MetricRow>,
    pub next: Option<MetricCursor>,
}

/// Bounded alert-transition page.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct AlertPage {
    pub transitions: Vec<AlertTransition>,
    pub next_after_sequence: Option<u64>,
}

/// Stable public error category without paths, SQL, or input echoes.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ObservabilityErrorKind {
    InvalidInput,
    Conflict,
    LimitExceeded,
    RuleSetChanged,
    ConfigurationChanged,
    CorruptState,
    Storage,
}

/// Secret-safe observability error.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ObservabilityError {
    kind: ObservabilityErrorKind,
}

impl ObservabilityError {
    pub const fn invalid() -> Self {
        Self {
            kind: ObservabilityErrorKind::InvalidInput,
        }
    }

    pub const fn conflict() -> Self {
        Self {
            kind: ObservabilityErrorKind::Conflict,
        }
    }

    pub const fn limit() -> Self {
        Self {
            kind: ObservabilityErrorKind::LimitExceeded,
        }
    }

    pub const fn corrupt() -> Self {
        Self {
            kind: ObservabilityErrorKind::CorruptState,
        }
    }

    pub const fn storage() -> Self {
        Self {
            kind: ObservabilityErrorKind::Storage,
        }
    }

    pub const fn rule_set_changed() -> Self {
        Self {
            kind: ObservabilityErrorKind::RuleSetChanged,
        }
    }

    pub const fn configuration_changed() -> Self {
        Self {
            kind: ObservabilityErrorKind::ConfigurationChanged,
        }
    }

    #[must_use]
    pub const fn kind(&self) -> ObservabilityErrorKind {
        self.kind
    }
}

impl fmt::Display for ObservabilityError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(match self.kind {
            ObservabilityErrorKind::InvalidInput => "observability input is invalid",
            ObservabilityErrorKind::Conflict => {
                "observability identity conflicts with durable state"
            }
            ObservabilityErrorKind::LimitExceeded => "observability bound is exceeded",
            ObservabilityErrorKind::RuleSetChanged => "observability alert rule set changed",
            ObservabilityErrorKind::ConfigurationChanged => {
                "observability durable configuration changed"
            }
            ObservabilityErrorKind::CorruptState => "observability durable state is corrupt",
            ObservabilityErrorKind::Storage => "observability storage failed",
        })
    }
}

impl std::error::Error for ObservabilityError {}

/// One storage-neutral metric update derived from a validated observation.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct MetricDelta {
    pub key: MetricSeriesKey,
    pub latency_millis: u64,
    pub recovered_items: u64,
    pub used: u64,
    pub limit: u64,
}

impl MetricDelta {
    /// Validates an observation and converts it into one closed metric update.
    ///
    /// # Errors
    ///
    /// Rejects malformed observations before deriving metric dimensions.
    pub fn try_from_observation(observation: &Observation) -> Result<Self, ObservabilityError> {
        observation.validate()?;
        Ok(match observation.signal {
            ObservationSignal::OperationCompleted {
                outcome,
                latency_millis,
            } => Self {
                key: MetricSeriesKey::Operation {
                    component: observation.component,
                    operation: observation.operation,
                    outcome,
                },
                latency_millis,
                recovered_items: 0,
                used: 0,
                limit: 0,
            },
            ObservationSignal::CapacityObserved {
                resource,
                used,
                limit,
            } => Self {
                key: MetricSeriesKey::Capacity {
                    component: observation.component,
                    resource,
                },
                latency_millis: 0,
                recovered_items: 0,
                used,
                limit,
            },
            ObservationSignal::RecoveryObserved {
                outcome,
                latency_millis,
                recovered_items,
            } => Self {
                key: MetricSeriesKey::Recovery {
                    component: observation.component,
                    operation: observation.operation,
                    outcome,
                },
                latency_millis,
                recovered_items,
                used: 0,
                limit: 0,
            },
            ObservationSignal::StructuredLog { severity, code } => Self {
                key: MetricSeriesKey::StructuredLog {
                    component: observation.component,
                    severity,
                    code,
                },
                latency_millis: 0,
                recovered_items: 0,
                used: 0,
                limit: 0,
            },
        })
    }
}

impl MetricRow {
    /// Starts one aggregate bucket from a validated observation.
    ///
    /// # Errors
    ///
    /// Rejects unsafe bucket or observation values.
    pub fn try_from_observation(
        bucket_start_unix_millis: u64,
        observation: &Observation,
    ) -> Result<Self, ObservabilityError> {
        safe_integer(bucket_start_unix_millis)?;
        let delta = MetricDelta::try_from_observation(observation)?;
        Ok(Self {
            bucket_start_unix_millis,
            key: delta.key,
            observations: 1,
            latency_total_millis: delta.latency_millis,
            latency_max_millis: delta.latency_millis,
            recovered_items: delta.recovered_items,
            latest_used: delta.used,
            latest_limit: delta.limit,
            maximum_used: delta.used,
        })
    }

    /// Applies another validated observation to the same closed metric series.
    ///
    /// # Errors
    ///
    /// Rejects a different series or an accumulator overflow.
    pub fn apply_observation(
        &mut self,
        observation: &Observation,
    ) -> Result<(), ObservabilityError> {
        let delta = MetricDelta::try_from_observation(observation)?;
        if self.key != delta.key {
            return Err(ObservabilityError::invalid());
        }
        self.observations = self
            .observations
            .checked_add(1)
            .ok_or_else(ObservabilityError::limit)?;
        self.latency_total_millis = self
            .latency_total_millis
            .checked_add(delta.latency_millis)
            .ok_or_else(ObservabilityError::limit)?;
        safe_integer(self.observations)?;
        safe_integer(self.latency_total_millis)?;
        self.latency_max_millis = self.latency_max_millis.max(delta.latency_millis);
        self.recovered_items = self
            .recovered_items
            .checked_add(delta.recovered_items)
            .ok_or_else(ObservabilityError::limit)?;
        safe_integer(self.recovered_items)?;
        self.latest_used = delta.used;
        self.latest_limit = delta.limit;
        self.maximum_used = self.maximum_used.max(delta.used);
        Ok(())
    }
}

/// Durable alert state without a storage-engine representation.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct AlertState {
    pub status: AlertStatus,
    pub generation: u64,
}

impl AlertState {
    /// Creates a validated durable state.
    ///
    /// # Errors
    ///
    /// Rejects zero or unsafe generations.
    pub fn try_new(status: AlertStatus, generation: u64) -> Result<Self, ObservabilityError> {
        if generation == 0 {
            return Err(ObservabilityError::corrupt());
        }
        safe_integer(generation).map_err(|_| ObservabilityError::corrupt())?;
        Ok(Self { status, generation })
    }
}

/// Evaluates one alert rule against an optional durable state.
///
/// `None` means the rule is either unrelated to the observation or remains in
/// its current state. A returned state must be persisted atomically with the
/// observation by the storage implementation.
///
/// # Errors
///
/// Rejects malformed rules, observations, or durable state.
pub fn evaluate_alert_rule(
    rule: &AlertRule,
    observation: &Observation,
    current: Option<AlertState>,
) -> Result<Option<AlertState>, ObservabilityError> {
    AlertRuleId::try_new(rule.rule_id.0.clone())?;
    rule.condition.validate()?;
    observation.validate()?;
    if let Some(state) = current {
        AlertState::try_new(state.status, state.generation)?;
    }
    let Some(firing) = rule.condition.evaluate(observation) else {
        return Ok(None);
    };
    let target = if firing {
        AlertStatus::Firing
    } else {
        AlertStatus::Resolved
    };
    let should_transition = matches!(
        (current.map(|state| state.status), target),
        (None | Some(AlertStatus::Resolved), AlertStatus::Firing)
            | (Some(AlertStatus::Firing), AlertStatus::Resolved)
    );
    if !should_transition {
        return Ok(None);
    }
    let generation = match (target, current) {
        (AlertStatus::Firing, Some(state)) => state
            .generation
            .checked_add(1)
            .ok_or_else(ObservabilityError::limit)?,
        (AlertStatus::Firing, None) => 1,
        (AlertStatus::Resolved, Some(state)) => state.generation,
        (AlertStatus::Resolved, None) => return Err(ObservabilityError::corrupt()),
    };
    safe_integer(generation)?;
    Ok(Some(AlertState {
        status: target,
        generation,
    }))
}

impl AlertTransition {
    /// Creates a deterministic transition after the store allocates a sequence.
    ///
    /// # Errors
    ///
    /// Rejects malformed sequence, rule, state, or observation values.
    pub fn try_from_state(
        sequence: u64,
        rule: &AlertRule,
        state: AlertState,
        observation: &Observation,
    ) -> Result<Self, ObservabilityError> {
        safe_integer(sequence)?;
        if sequence == 0 {
            return Err(ObservabilityError::invalid());
        }
        AlertRuleId::try_new(rule.rule_id.0.clone())?;
        let state = AlertState::try_new(state.status, state.generation)?;
        observation.validate()?;
        let transition = Self {
            sequence,
            alert_id: alert_id(&rule.rule_id, state.generation),
            rule_id: rule.rule_id.clone(),
            severity: rule.severity,
            status: state.status,
            generation: state.generation,
            trace: observation.trace.clone(),
            observation_id: observation.observation_id.clone(),
            occurred_at_unix_millis: observation.occurred_at_unix_millis,
        };
        validate_alert_transition(&transition).map_err(|_| ObservabilityError::invalid())?;
        Ok(transition)
    }
}

/// Storage-neutral recording and bounded-query port.
pub trait ObservabilityStore {
    /// Records one validated observation atomically.
    ///
    /// # Errors
    ///
    /// Returns a stable validation, conflict, limit, corruption, or storage error.
    fn record(
        &mut self,
        observation: &Observation,
    ) -> Result<ObservationReceipt, ObservabilityError>;

    /// Reads one bounded trace page.
    ///
    /// # Errors
    ///
    /// Returns a stable validation, limit, corruption, or storage error.
    fn trace_page(
        &self,
        trace_id: &TraceId,
        after_sequence: u64,
        limit: u32,
    ) -> Result<TracePage, ObservabilityError>;

    /// Reads one bounded metric page.
    ///
    /// # Errors
    ///
    /// Returns a stable validation, limit, corruption, or storage error.
    fn metric_page(
        &self,
        from_bucket_inclusive: u64,
        to_bucket_exclusive: u64,
        after: Option<&MetricCursor>,
        limit: u32,
    ) -> Result<MetricPage, ObservabilityError>;

    /// Reads one bounded alert-transition page.
    ///
    /// # Errors
    ///
    /// Returns a stable validation, limit, corruption, or storage error.
    fn alert_page(&self, after_sequence: u64, limit: u32) -> Result<AlertPage, ObservabilityError>;
}

/// Validates a bounded metric query window.
///
/// # Errors
///
/// Rejects unsafe, unaligned, reversed, or oversized windows.
pub fn validate_bucket_window(
    config: &ObservabilityConfig,
    from: u64,
    to: u64,
) -> Result<(), ObservabilityError> {
    safe_integer(from)?;
    safe_integer(to)?;
    if from >= to
        || !from.is_multiple_of(config.bucket_width_millis)
        || !to.is_multiple_of(config.bucket_width_millis)
        || (to - from) / config.bucket_width_millis > u64::from(config.max_query_buckets)
    {
        return Err(ObservabilityError::invalid());
    }
    Ok(())
}

/// Validates one bounded page limit.
///
/// # Errors
///
/// Rejects zero or values above the configured limit.
pub fn validate_limit(limit: u32, configured: u32) -> Result<(), ObservabilityError> {
    if limit == 0 || limit > configured {
        return Err(ObservabilityError::limit());
    }
    Ok(())
}

fn operation_belongs_to(component: Component, operation: Operation) -> bool {
    matches!(
        (component, operation),
        (Component::Http, Operation::HttpRequest)
            | (
                Component::WebSocket,
                Operation::WebSocketConnect | Operation::WebSocketPublish
            )
            | (
                Component::Scheduler,
                Operation::SchedulerTick | Operation::SchedulerDispatch
            )
            | (
                Component::Worker,
                Operation::WorkerHeartbeat | Operation::WorkerLease | Operation::WorkerRecovery
            )
            | (
                Component::Provider,
                Operation::ProviderOpen | Operation::ProviderStream | Operation::ProviderSettlement
            )
            | (
                Component::Storage,
                Operation::StorageRead | Operation::StorageWrite
            )
            | (
                Component::Queue,
                Operation::QueueEnqueue | Operation::QueueDequeue | Operation::QueueRetry
            )
    )
}

fn resource_belongs_to(component: Component, resource: CapacityResource) -> bool {
    matches!(
        (component, resource),
        (Component::Http, CapacityResource::HttpInFlight)
            | (Component::WebSocket, CapacityResource::WebSocketConnections)
            | (Component::Scheduler, CapacityResource::SchedulerReadyJobs)
            | (Component::Worker, CapacityResource::WorkerAvailableSlots)
            | (Component::Provider, CapacityResource::ProviderInFlight)
            | (Component::Storage, CapacityResource::StorageBusyWriters)
            | (Component::Queue, CapacityResource::QueueDepth)
    )
}

fn diagnostic_belongs_to(component: Component, code: DiagnosticCode) -> bool {
    matches!(
        (component, code),
        (Component::Http, DiagnosticCode::RequestRejected)
            | (Component::WebSocket, DiagnosticCode::ConnectionClosed)
            | (Component::Scheduler, DiagnosticCode::SchedulerStalled)
            | (Component::Worker, DiagnosticCode::WorkerHeartbeatMissed)
            | (Component::Provider, DiagnosticCode::ProviderRateLimited)
            | (Component::Storage, DiagnosticCode::StorageBusy)
            | (Component::Queue, DiagnosticCode::QueueBacklog)
            | (
                Component::Worker | Component::Queue,
                DiagnosticCode::RecoveryStarted
                    | DiagnosticCode::RecoveryCompleted
                    | DiagnosticCode::RecoveryFailed
            )
    )
}

/// Validates a durable alert transition loaded by a storage implementation.
///
/// # Errors
///
/// Rejects malformed identities, sequences, generations, traces, or timestamps.
pub fn validate_alert_transition(transition: &AlertTransition) -> Result<(), ObservabilityError> {
    if transition.sequence == 0
        || transition.generation == 0
        || !canonical_id(&transition.alert_id, "alr")
    {
        return Err(ObservabilityError::corrupt());
    }
    AlertRuleId::try_new(transition.rule_id.0.clone())
        .map_err(|_| ObservabilityError::corrupt())?;
    ObservationId::try_new(transition.observation_id.0.clone())
        .map_err(|_| ObservabilityError::corrupt())?;
    transition
        .trace
        .validate()
        .map_err(|_| ObservabilityError::corrupt())?;
    safe_integer(transition.sequence).map_err(|_| ObservabilityError::corrupt())?;
    safe_integer(transition.generation).map_err(|_| ObservabilityError::corrupt())?;
    safe_integer(transition.occurred_at_unix_millis).map_err(|_| ObservabilityError::corrupt())
}

fn alert_id(rule_id: &AlertRuleId, generation: u64) -> String {
    let material = format!(
        "winwincode-observability-alert-v1\0{}\0{generation}",
        rule_id.as_str()
    );
    let digest = Sha256::digest(material.as_bytes());
    format!("alr_{}", crockford_128(&digest[..16]))
}

fn crockford_128(bytes: &[u8]) -> String {
    const ALPHABET: &[u8; 32] = b"0123456789ABCDEFGHJKMNPQRSTVWXYZ";
    let mut raw = [0_u8; 16];
    raw.copy_from_slice(bytes);
    let mut value = u128::from_be_bytes(raw);
    let mut encoded = [b'0'; 26];
    for byte in encoded.iter_mut().rev() {
        *byte = ALPHABET[(value & 31) as usize];
        value >>= 5;
    }
    encoded.into_iter().map(char::from).collect()
}

/// Validates a value against the shared exact-integer bound.
///
/// # Errors
///
/// Rejects values above the shared exact-integer maximum.
pub fn safe_integer(value: u64) -> Result<(), ObservabilityError> {
    if value > MAX_SAFE_INTEGER {
        return Err(ObservabilityError::invalid());
    }
    Ok(())
}

fn sha256_digest(value: &str) -> bool {
    value.len() == 71
        && value.starts_with("sha256:")
        && value[7..]
            .bytes()
            .all(|byte| byte.is_ascii_digit() || matches!(byte, b'a'..=b'f'))
}

fn lower_hex(value: &str, length: usize) -> bool {
    value.len() == length
        && value
            .bytes()
            .all(|byte| byte.is_ascii_digit() || matches!(byte, b'a'..=b'f'))
}

fn canonical_id(value: &str, prefix: &str) -> bool {
    value
        .strip_prefix(prefix)
        .and_then(|suffix| suffix.strip_prefix('_'))
        .is_some_and(|suffix| {
            suffix.len() == 26
                && suffix.bytes().all(|byte| {
                    byte.is_ascii_digit()
                        || matches!(byte, b'A'..=b'H' | b'J' | b'K' | b'M' | b'N' | b'P'..=b'T' | b'V'..=b'Z')
                })
        })
}

const fn secret_markers() -> &'static [&'static str] {
    &[
        "authorization",
        "bearer",
        "password",
        "secret",
        "token",
        "api_key",
        "apikey",
        "private_key",
        "private-key",
    ]
}
