// SPDX-License-Identifier: Apache-2.0

//! Bounded, secret-safe operational telemetry for `WinWinCode` processes.
//!
//! This crate accepts only closed metric dimensions, stable correlation
//! identities, and references to facts that their owning subsystem already
//! validated. It never accepts prompts, request bodies, credential material,
//! command arguments, file contents, or arbitrary log messages. Telemetry is
//! diagnostic state only and never becomes a business authority.

use std::{collections::BTreeSet, fmt, fs, path::Path, time::Duration};

use rusqlite::{Connection, OptionalExtension, Transaction, TransactionBehavior, params};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

const SCHEMA_VERSION: &str = "winwincode.observability.sqlite.v1";
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

    fn validate(&self) -> Result<(), ObservabilityError> {
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
    fn validate(&self) -> Result<(), ObservabilityError> {
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

    fn validate_for_config(&self, config: &ObservabilityConfig) -> Result<(), ObservabilityError> {
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
    fn validate_and_normalize(&mut self) -> Result<(), ObservabilityError> {
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
    fn validate(&self) -> Result<(), ObservabilityError> {
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
    const fn invalid() -> Self {
        Self {
            kind: ObservabilityErrorKind::InvalidInput,
        }
    }

    const fn conflict() -> Self {
        Self {
            kind: ObservabilityErrorKind::Conflict,
        }
    }

    const fn limit() -> Self {
        Self {
            kind: ObservabilityErrorKind::LimitExceeded,
        }
    }

    const fn corrupt() -> Self {
        Self {
            kind: ObservabilityErrorKind::CorruptState,
        }
    }

    const fn storage() -> Self {
        Self {
            kind: ObservabilityErrorKind::Storage,
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

/// SQLite-backed telemetry service. Each operation is bounded and owns its
/// transaction; no query returns a live cursor or holds a lock after return.
pub struct SqliteObservability {
    connection: Connection,
    config: ObservabilityConfig,
}

impl SqliteObservability {
    /// Opens or creates a durable observability database in WAL mode.
    ///
    /// # Errors
    ///
    /// Returns a secret-safe error for invalid configuration, inaccessible
    /// storage, schema mismatch, or an alert rule set that changed after the
    /// database was initialized.
    pub fn open(
        path: impl AsRef<Path>,
        mut config: ObservabilityConfig,
    ) -> Result<Self, ObservabilityError> {
        config.validate_and_normalize()?;
        let path = path.as_ref();
        prepare_parent(path)?;
        let mut connection = Connection::open(path).map_err(|_| ObservabilityError::storage())?;
        connection
            .busy_timeout(Duration::from_secs(5))
            .map_err(|_| ObservabilityError::storage())?;
        connection
            .execute_batch(
                "PRAGMA journal_mode=WAL;
                 PRAGMA synchronous=FULL;
                 PRAGMA foreign_keys=ON;
                 PRAGMA trusted_schema=OFF;",
            )
            .map_err(|_| ObservabilityError::storage())?;
        protect_database(path)?;
        initialize_schema(&connection)?;
        validate_metadata(&mut connection, &config)?;
        validate_metric_counter(&connection)?;
        Ok(Self { connection, config })
    }

    /// Atomically records a structured observation, updates its fixed metric
    /// bucket, and evaluates durable alert state.
    ///
    /// Exact replay returns the original transition set without updating
    /// metrics or alerts. Reusing an observation or source identity with
    /// changed facts fails before any write.
    ///
    /// # Errors
    ///
    /// Returns a stable error for invalid input, replay conflict, configured
    /// receipt exhaustion, corrupt durable rows, or storage failure.
    pub fn record(
        &mut self,
        observation: &Observation,
    ) -> Result<ObservationReceipt, ObservabilityError> {
        observation.validate()?;
        observation.validate_for_config(&self.config)?;
        let observation_json =
            serde_json::to_vec(observation).map_err(|_| ObservabilityError::invalid())?;
        let body_digest = sha256(&observation_json);
        let transaction = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(|_| ObservabilityError::storage())?;
        if let Some(receipt) = replay_receipt(&transaction, observation, &body_digest)? {
            transaction
                .commit()
                .map_err(|_| ObservabilityError::storage())?;
            return Ok(receipt);
        }
        reject_source_reuse(&transaction, observation)?;
        enforce_receipt_bound(&transaction, self.config.max_receipts)?;

        let sequence = insert_receipt(&transaction, observation, &body_digest)?;
        insert_trace_row(&transaction, sequence, observation, &observation_json)?;
        update_metric(&transaction, &self.config, observation)?;
        let transitions = evaluate_alerts(&transaction, &self.config.alert_rules, observation)?;
        store_receipt_transitions(&transaction, sequence, &transitions)?;
        trim_trace_rows(&transaction, self.config.max_trace_rows, sequence)?;
        trim_metric_rows(&transaction, self.config.max_metric_rows)?;
        transaction
            .commit()
            .map_err(|_| ObservabilityError::storage())?;
        Ok(ObservationReceipt {
            accepted_sequence: sequence,
            duplicate: false,
            alert_transitions: transitions,
        })
    }

    /// Loads one bounded materialized trace page.
    ///
    /// # Errors
    ///
    /// Rejects malformed trace identities, zero or oversized limits, and
    /// corrupt retained observations.
    pub fn trace_page(
        &self,
        trace_id: &TraceId,
        after_sequence: u64,
        limit: u32,
    ) -> Result<TracePage, ObservabilityError> {
        TraceId::try_new(trace_id.0.clone())?;
        validate_limit(limit, self.config.max_query_rows)?;
        safe_integer(after_sequence)?;
        let fetch_limit = i64::from(limit) + 1;
        let mut statement = self
            .connection
            .prepare(
                "SELECT sequence, observation_json
                 FROM observation_log
                 WHERE trace_id = ?1 AND sequence > ?2
                 ORDER BY sequence ASC
                 LIMIT ?3",
            )
            .map_err(|_| ObservabilityError::storage())?;
        let mut rows = statement
            .query(params![
                trace_id.as_str(),
                to_i64(after_sequence)?,
                fetch_limit
            ])
            .map_err(|_| ObservabilityError::storage())?;
        let mut result = Vec::with_capacity(
            usize::try_from(limit).map_err(|_| ObservabilityError::limit())? + 1,
        );
        while let Some(row) = rows.next().map_err(|_| ObservabilityError::storage())? {
            let sequence = from_i64(row.get(0).map_err(|_| ObservabilityError::corrupt())?)?;
            let json: Vec<u8> = row.get(1).map_err(|_| ObservabilityError::corrupt())?;
            let observation: Observation =
                serde_json::from_slice(&json).map_err(|_| ObservabilityError::corrupt())?;
            observation
                .validate()
                .map_err(|_| ObservabilityError::corrupt())?;
            result.push(TraceRow {
                sequence,
                observation,
            });
        }
        let has_more =
            result.len() > usize::try_from(limit).map_err(|_| ObservabilityError::limit())?;
        if has_more {
            result.pop();
        }
        let next_after_sequence = has_more
            .then(|| result.last().map(|row| row.sequence))
            .flatten();
        Ok(TracePage {
            rows: result,
            next_after_sequence,
        })
    }

    /// Loads a bounded metric page over an explicitly bounded bucket window.
    ///
    /// # Errors
    ///
    /// Rejects non-aligned or oversized windows, malformed cursors, oversized
    /// pages, and corrupt metric rows.
    pub fn metric_page(
        &self,
        from_bucket_inclusive: u64,
        to_bucket_exclusive: u64,
        after: Option<&MetricCursor>,
        limit: u32,
    ) -> Result<MetricPage, ObservabilityError> {
        validate_limit(limit, self.config.max_query_rows)?;
        validate_bucket_window(&self.config, from_bucket_inclusive, to_bucket_exclusive)?;
        let (after_bucket, after_key) = match after {
            Some(cursor) => {
                if cursor.bucket_start_unix_millis < from_bucket_inclusive
                    || cursor.bucket_start_unix_millis >= to_bucket_exclusive
                {
                    return Err(ObservabilityError::invalid());
                }
                cursor.key.validate()?;
                (
                    cursor.bucket_start_unix_millis,
                    serde_json::to_string(&cursor.key)
                        .map_err(|_| ObservabilityError::invalid())?,
                )
            }
            None => (from_bucket_inclusive, String::new()),
        };
        let mut statement = self
            .connection
            .prepare(
                "SELECT bucket_start_millis, series_key, observations,
                        latency_total_millis, latency_max_millis, recovered_items,
                        latest_used, latest_limit, maximum_used
                 FROM metric_series
                 WHERE bucket_start_millis >= ?1 AND bucket_start_millis < ?2
                   AND (bucket_start_millis > ?3 OR
                        (bucket_start_millis = ?3 AND series_key > ?4))
                 ORDER BY bucket_start_millis ASC, series_key ASC
                 LIMIT ?5",
            )
            .map_err(|_| ObservabilityError::storage())?;
        let fetch_limit = i64::from(limit) + 1;
        let mut rows = statement
            .query(params![
                to_i64(from_bucket_inclusive)?,
                to_i64(to_bucket_exclusive)?,
                to_i64(after_bucket)?,
                after_key,
                fetch_limit
            ])
            .map_err(|_| ObservabilityError::storage())?;
        let mut result = Vec::with_capacity(
            usize::try_from(limit).map_err(|_| ObservabilityError::limit())? + 1,
        );
        while let Some(row) = rows.next().map_err(|_| ObservabilityError::storage())? {
            result.push(decode_metric_row(row)?);
        }
        let has_more =
            result.len() > usize::try_from(limit).map_err(|_| ObservabilityError::limit())?;
        if has_more {
            result.pop();
        }
        let next = has_more
            .then(|| {
                result.last().map(|row| MetricCursor {
                    bucket_start_unix_millis: row.bucket_start_unix_millis,
                    key: row.key.clone(),
                })
            })
            .flatten();
        Ok(MetricPage { rows: result, next })
    }

    /// Loads a bounded alert-transition page.
    ///
    /// # Errors
    ///
    /// Rejects oversized limits and corrupt durable transitions.
    pub fn alert_page(
        &self,
        after_sequence: u64,
        limit: u32,
    ) -> Result<AlertPage, ObservabilityError> {
        validate_limit(limit, self.config.max_query_rows)?;
        safe_integer(after_sequence)?;
        let mut statement = self
            .connection
            .prepare(
                "SELECT transition_json
                 FROM alert_transitions
                 WHERE sequence > ?1
                 ORDER BY sequence ASC
                 LIMIT ?2",
            )
            .map_err(|_| ObservabilityError::storage())?;
        let fetch_limit = i64::from(limit) + 1;
        let mut rows = statement
            .query(params![to_i64(after_sequence)?, fetch_limit])
            .map_err(|_| ObservabilityError::storage())?;
        let mut result = Vec::with_capacity(
            usize::try_from(limit).map_err(|_| ObservabilityError::limit())? + 1,
        );
        while let Some(row) = rows.next().map_err(|_| ObservabilityError::storage())? {
            let json: Vec<u8> = row.get(0).map_err(|_| ObservabilityError::corrupt())?;
            let transition: AlertTransition =
                serde_json::from_slice(&json).map_err(|_| ObservabilityError::corrupt())?;
            validate_alert_transition(&transition)?;
            result.push(transition);
        }
        let has_more =
            result.len() > usize::try_from(limit).map_err(|_| ObservabilityError::limit())?;
        if has_more {
            result.pop();
        }
        let next_after_sequence = has_more
            .then(|| result.last().map(|row| row.sequence))
            .flatten();
        Ok(AlertPage {
            transitions: result,
            next_after_sequence,
        })
    }
}

fn initialize_schema(connection: &Connection) -> Result<(), ObservabilityError> {
    connection
        .execute_batch(
            "CREATE TABLE IF NOT EXISTS observability_metadata (
                 key TEXT PRIMARY KEY NOT NULL,
                 value BLOB NOT NULL
             ) STRICT;
             CREATE TABLE IF NOT EXISTS observation_receipts (
                 sequence INTEGER PRIMARY KEY AUTOINCREMENT,
                 observation_id TEXT UNIQUE NOT NULL,
                 body_digest TEXT NOT NULL,
                 source_kind TEXT NOT NULL,
                 source_id TEXT NOT NULL,
                 source_digest TEXT NOT NULL,
                 alert_transitions_json BLOB NOT NULL,
                 UNIQUE(source_kind, source_id)
             ) STRICT;
             CREATE TABLE IF NOT EXISTS observation_log (
                 sequence INTEGER PRIMARY KEY NOT NULL,
                 trace_id TEXT NOT NULL,
                 occurred_at_millis INTEGER NOT NULL,
                 observation_json BLOB NOT NULL,
                 FOREIGN KEY(sequence) REFERENCES observation_receipts(sequence)
             ) STRICT;
             CREATE INDEX IF NOT EXISTS observation_log_trace_sequence
                 ON observation_log(trace_id, sequence);
             CREATE TABLE IF NOT EXISTS metric_series (
                 bucket_start_millis INTEGER NOT NULL,
                 series_key TEXT NOT NULL,
                 observations INTEGER NOT NULL,
                 latency_total_millis INTEGER NOT NULL,
                 latency_max_millis INTEGER NOT NULL,
                 recovered_items INTEGER NOT NULL,
                 latest_used INTEGER NOT NULL,
                 latest_limit INTEGER NOT NULL,
                 maximum_used INTEGER NOT NULL,
                 PRIMARY KEY(bucket_start_millis, series_key)
             ) STRICT, WITHOUT ROWID;
             CREATE TABLE IF NOT EXISTS alert_states (
                 rule_id TEXT PRIMARY KEY NOT NULL,
                 status TEXT NOT NULL,
                 generation INTEGER NOT NULL
             ) STRICT;
             CREATE TABLE IF NOT EXISTS alert_transitions (
                 sequence INTEGER PRIMARY KEY AUTOINCREMENT,
                 rule_id TEXT NOT NULL,
                 generation INTEGER NOT NULL,
                 status TEXT NOT NULL,
                 transition_json BLOB NOT NULL
             ) STRICT;
             CREATE TABLE IF NOT EXISTS observability_counters (
                 key TEXT PRIMARY KEY NOT NULL,
                 value INTEGER NOT NULL CHECK(value >= 0)
             ) STRICT;
             INSERT OR IGNORE INTO observability_counters(key, value)
                 VALUES ('metric_rows', 0);",
        )
        .map_err(|_| ObservabilityError::storage())
}

fn validate_metadata(
    connection: &mut Connection,
    config: &ObservabilityConfig,
) -> Result<(), ObservabilityError> {
    let rule_json =
        serde_json::to_vec(&config.alert_rules).map_err(|_| ObservabilityError::invalid())?;
    let rules_digest = sha256(&rule_json);
    let bucket_width = config.bucket_width_millis.to_string();
    let transaction = connection
        .transaction_with_behavior(TransactionBehavior::Immediate)
        .map_err(|_| ObservabilityError::storage())?;
    let existing_schema: Option<Vec<u8>> = transaction
        .query_row(
            "SELECT value FROM observability_metadata WHERE key = 'schema_version'",
            [],
            |row| row.get(0),
        )
        .optional()
        .map_err(|_| ObservabilityError::storage())?;
    let existing_rules: Option<Vec<u8>> = transaction
        .query_row(
            "SELECT value FROM observability_metadata WHERE key = 'alert_rules_digest'",
            [],
            |row| row.get(0),
        )
        .optional()
        .map_err(|_| ObservabilityError::storage())?;
    let existing_bucket_width: Option<Vec<u8>> = transaction
        .query_row(
            "SELECT value FROM observability_metadata WHERE key = 'bucket_width_millis'",
            [],
            |row| row.get(0),
        )
        .optional()
        .map_err(|_| ObservabilityError::storage())?;
    let validation = match (existing_schema, existing_rules, existing_bucket_width) {
        (None, None, None) => {
            transaction
                .execute(
                    "INSERT INTO observability_metadata(key, value) VALUES ('schema_version', ?1)",
                    [SCHEMA_VERSION.as_bytes()],
                )
                .map_err(|_| ObservabilityError::storage())?;
            transaction
                .execute(
                    "INSERT INTO observability_metadata(key, value) VALUES ('alert_rules_digest', ?1)",
                    [rules_digest.as_bytes()],
                )
                .map_err(|_| ObservabilityError::storage())?;
            transaction
                .execute(
                    "INSERT INTO observability_metadata(key, value) VALUES ('bucket_width_millis', ?1)",
                    [bucket_width.as_bytes()],
                )
                .map_err(|_| ObservabilityError::storage())?;
            Ok(())
        }
        (Some(schema), Some(rules), Some(stored_bucket_width))
            if schema == SCHEMA_VERSION.as_bytes() =>
        {
            if rules != rules_digest.as_bytes() {
                Err(ObservabilityError {
                    kind: ObservabilityErrorKind::RuleSetChanged,
                })
            } else if stored_bucket_width != bucket_width.as_bytes() {
                Err(ObservabilityError {
                    kind: ObservabilityErrorKind::ConfigurationChanged,
                })
            } else {
                Ok(())
            }
        }
        _ => Err(ObservabilityError::corrupt()),
    };
    validation?;
    transaction
        .commit()
        .map_err(|_| ObservabilityError::storage())
}

fn validate_metric_counter(connection: &Connection) -> Result<(), ObservabilityError> {
    let stored: i64 = connection
        .query_row(
            "SELECT value FROM observability_counters WHERE key = 'metric_rows'",
            [],
            |row| row.get(0),
        )
        .map_err(|_| ObservabilityError::corrupt())?;
    let actual: i64 = connection
        .query_row("SELECT COUNT(*) FROM metric_series", [], |row| row.get(0))
        .map_err(|_| ObservabilityError::corrupt())?;
    if stored == actual {
        Ok(())
    } else {
        Err(ObservabilityError::corrupt())
    }
}

fn replay_receipt(
    transaction: &Transaction<'_>,
    observation: &Observation,
    body_digest: &str,
) -> Result<Option<ObservationReceipt>, ObservabilityError> {
    let stored: Option<(String, i64, Vec<u8>)> = transaction
        .query_row(
            "SELECT body_digest, sequence, alert_transitions_json
             FROM observation_receipts WHERE observation_id = ?1",
            [observation.observation_id.as_str()],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
        )
        .optional()
        .map_err(|_| ObservabilityError::storage())?;
    let Some((stored_digest, sequence, transitions_json)) = stored else {
        return Ok(None);
    };
    if stored_digest != body_digest {
        return Err(ObservabilityError::conflict());
    }
    let transitions: Vec<AlertTransition> =
        serde_json::from_slice(&transitions_json).map_err(|_| ObservabilityError::corrupt())?;
    for transition in &transitions {
        validate_alert_transition(transition)?;
    }
    Ok(Some(ObservationReceipt {
        accepted_sequence: from_i64(sequence)?,
        duplicate: true,
        alert_transitions: transitions,
    }))
}

fn reject_source_reuse(
    transaction: &Transaction<'_>,
    observation: &Observation,
) -> Result<(), ObservabilityError> {
    let source_kind = serde_json::to_string(&observation.source.kind)
        .map_err(|_| ObservabilityError::invalid())?;
    let existing: Option<(String, String)> = transaction
        .query_row(
            "SELECT observation_id, source_digest FROM observation_receipts
             WHERE source_kind = ?1 AND source_id = ?2",
            params![source_kind, observation.source.fact_id.as_str()],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .optional()
        .map_err(|_| ObservabilityError::storage())?;
    if existing.is_some() {
        return Err(ObservabilityError::conflict());
    }
    Ok(())
}

fn enforce_receipt_bound(
    transaction: &Transaction<'_>,
    max_receipts: u64,
) -> Result<(), ObservabilityError> {
    let highest_sequence: i64 = transaction
        .query_row(
            "SELECT COALESCE(MAX(sequence), 0) FROM observation_receipts",
            [],
            |row| row.get(0),
        )
        .map_err(|_| ObservabilityError::storage())?;
    if from_i64(highest_sequence)? >= max_receipts {
        return Err(ObservabilityError::limit());
    }
    Ok(())
}

fn insert_receipt(
    transaction: &Transaction<'_>,
    observation: &Observation,
    body_digest: &str,
) -> Result<u64, ObservabilityError> {
    let source_kind = serde_json::to_string(&observation.source.kind)
        .map_err(|_| ObservabilityError::invalid())?;
    transaction
        .execute(
            "INSERT INTO observation_receipts(
                 observation_id, body_digest, source_kind, source_id, source_digest,
                 alert_transitions_json
             ) VALUES (?1, ?2, ?3, ?4, ?5, X'5B5D')",
            params![
                observation.observation_id.as_str(),
                body_digest,
                source_kind,
                observation.source.fact_id.as_str(),
                observation.source.fact_digest.as_str()
            ],
        )
        .map_err(|_| ObservabilityError::storage())?;
    from_i64(transaction.last_insert_rowid())
}

fn insert_trace_row(
    transaction: &Transaction<'_>,
    sequence: u64,
    observation: &Observation,
    observation_json: &[u8],
) -> Result<(), ObservabilityError> {
    transaction
        .execute(
            "INSERT INTO observation_log(
                 sequence, trace_id, occurred_at_millis, observation_json
             ) VALUES (?1, ?2, ?3, ?4)",
            params![
                to_i64(sequence)?,
                observation.trace.trace_id.as_str(),
                to_i64(observation.occurred_at_unix_millis)?,
                observation_json
            ],
        )
        .map_err(|_| ObservabilityError::storage())?;
    Ok(())
}

fn update_metric(
    transaction: &Transaction<'_>,
    config: &ObservabilityConfig,
    observation: &Observation,
) -> Result<(), ObservabilityError> {
    let bucket_start = observation.occurred_at_unix_millis
        - observation.occurred_at_unix_millis % config.bucket_width_millis;
    let update = MetricUpdate::from_observation(observation);
    let key_json = serde_json::to_string(&update.key).map_err(|_| ObservabilityError::invalid())?;
    let existed: bool = transaction
        .query_row(
            "SELECT EXISTS(
                 SELECT 1 FROM metric_series
                 WHERE bucket_start_millis = ?1 AND series_key = ?2
             )",
            params![to_i64(bucket_start)?, key_json],
            |row| row.get(0),
        )
        .map_err(|_| ObservabilityError::storage())?;
    transaction
        .execute(
            "INSERT INTO metric_series(
                 bucket_start_millis, series_key, observations,
                 latency_total_millis, latency_max_millis, recovered_items,
                 latest_used, latest_limit, maximum_used
             ) VALUES (?1, ?2, 1, ?3, ?4, ?5, ?6, ?7, ?8)
             ON CONFLICT(bucket_start_millis, series_key) DO UPDATE SET
                 observations = observations + 1,
                 latency_total_millis = latency_total_millis + excluded.latency_total_millis,
                 latency_max_millis = MAX(latency_max_millis, excluded.latency_max_millis),
                 recovered_items = recovered_items + excluded.recovered_items,
                 latest_used = excluded.latest_used,
                 latest_limit = excluded.latest_limit,
                 maximum_used = MAX(maximum_used, excluded.maximum_used)",
            params![
                to_i64(bucket_start)?,
                key_json,
                to_i64(update.latency_millis)?,
                to_i64(update.latency_millis)?,
                to_i64(update.recovered_items)?,
                to_i64(update.used)?,
                to_i64(update.limit)?,
                to_i64(update.used)?
            ],
        )
        .map_err(|_| ObservabilityError::storage())?;
    if !existed {
        let changed = transaction
            .execute(
                "UPDATE observability_counters SET value = value + 1
                 WHERE key = 'metric_rows'",
                [],
            )
            .map_err(|_| ObservabilityError::storage())?;
        if changed != 1 {
            return Err(ObservabilityError::corrupt());
        }
    }
    Ok(())
}

struct MetricUpdate {
    key: MetricSeriesKey,
    latency_millis: u64,
    recovered_items: u64,
    used: u64,
    limit: u64,
}

impl MetricUpdate {
    fn from_observation(observation: &Observation) -> Self {
        match observation.signal {
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
        }
    }
}

fn evaluate_alerts(
    transaction: &Transaction<'_>,
    rules: &[AlertRule],
    observation: &Observation,
) -> Result<Vec<AlertTransition>, ObservabilityError> {
    let mut transitions = Vec::new();
    for rule in rules {
        let Some(firing) = rule.condition.evaluate(observation) else {
            continue;
        };
        let state: Option<(String, i64)> = transaction
            .query_row(
                "SELECT status, generation FROM alert_states WHERE rule_id = ?1",
                [rule.rule_id.as_str()],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .optional()
            .map_err(|_| ObservabilityError::storage())?;
        let (current, generation) = match state {
            Some((status, generation)) => {
                (Some(parse_alert_status(&status)?), from_i64(generation)?)
            }
            None => (None, 0),
        };
        let target = if firing {
            AlertStatus::Firing
        } else {
            AlertStatus::Resolved
        };
        let should_transition = matches!(
            (current, target),
            (None | Some(AlertStatus::Resolved), AlertStatus::Firing)
                | (Some(AlertStatus::Firing), AlertStatus::Resolved)
        );
        if !should_transition {
            continue;
        }
        let next_generation = if target == AlertStatus::Firing {
            generation
                .checked_add(1)
                .ok_or_else(ObservabilityError::limit)?
        } else {
            generation
        };
        transaction
            .execute(
                "INSERT INTO alert_states(rule_id, status, generation)
                 VALUES (?1, ?2, ?3)
                 ON CONFLICT(rule_id) DO UPDATE SET
                     status = excluded.status,
                     generation = excluded.generation",
                params![
                    rule.rule_id.as_str(),
                    alert_status_key(target),
                    to_i64(next_generation)?
                ],
            )
            .map_err(|_| ObservabilityError::storage())?;
        transaction
            .execute(
                "INSERT INTO alert_transitions(rule_id, generation, status, transition_json)
                 VALUES (?1, ?2, ?3, X'7B7D')",
                params![
                    rule.rule_id.as_str(),
                    to_i64(next_generation)?,
                    alert_status_key(target)
                ],
            )
            .map_err(|_| ObservabilityError::storage())?;
        let sequence = from_i64(transaction.last_insert_rowid())?;
        let transition = AlertTransition {
            sequence,
            alert_id: alert_id(&rule.rule_id, next_generation),
            rule_id: rule.rule_id.clone(),
            severity: rule.severity,
            status: target,
            generation: next_generation,
            trace: observation.trace.clone(),
            observation_id: observation.observation_id.clone(),
            occurred_at_unix_millis: observation.occurred_at_unix_millis,
        };
        let transition_json =
            serde_json::to_vec(&transition).map_err(|_| ObservabilityError::invalid())?;
        transaction
            .execute(
                "UPDATE alert_transitions SET transition_json = ?1 WHERE sequence = ?2",
                params![transition_json, to_i64(sequence)?],
            )
            .map_err(|_| ObservabilityError::storage())?;
        transitions.push(transition);
    }
    Ok(transitions)
}

fn store_receipt_transitions(
    transaction: &Transaction<'_>,
    sequence: u64,
    transitions: &[AlertTransition],
) -> Result<(), ObservabilityError> {
    let json = serde_json::to_vec(transitions).map_err(|_| ObservabilityError::invalid())?;
    transaction
        .execute(
            "UPDATE observation_receipts SET alert_transitions_json = ?1 WHERE sequence = ?2",
            params![json, to_i64(sequence)?],
        )
        .map_err(|_| ObservabilityError::storage())?;
    Ok(())
}

fn trim_trace_rows(
    transaction: &Transaction<'_>,
    max_trace_rows: u64,
    current_sequence: u64,
) -> Result<(), ObservabilityError> {
    let cutoff = current_sequence.saturating_sub(max_trace_rows);
    if cutoff > 0 {
        transaction
            .execute(
                "DELETE FROM observation_log WHERE sequence <= ?1",
                [to_i64(cutoff)?],
            )
            .map_err(|_| ObservabilityError::storage())?;
    }
    Ok(())
}

fn trim_metric_rows(
    transaction: &Transaction<'_>,
    max_metric_rows: u64,
) -> Result<(), ObservabilityError> {
    let count: i64 = transaction
        .query_row(
            "SELECT value FROM observability_counters WHERE key = 'metric_rows'",
            [],
            |row| row.get(0),
        )
        .map_err(|_| ObservabilityError::storage())?;
    let excess = from_i64(count)?.saturating_sub(max_metric_rows);
    if excess > 0 {
        let deleted = transaction
            .execute(
                "DELETE FROM metric_series WHERE (bucket_start_millis, series_key) IN (
                     SELECT bucket_start_millis, series_key FROM metric_series
                     ORDER BY bucket_start_millis ASC, series_key ASC LIMIT ?1
                 )",
                [to_i64(excess)?],
            )
            .map_err(|_| ObservabilityError::storage())?;
        if u64::try_from(deleted).map_err(|_| ObservabilityError::corrupt())? != excess {
            return Err(ObservabilityError::corrupt());
        }
        let changed = transaction
            .execute(
                "UPDATE observability_counters SET value = value - ?1
                 WHERE key = 'metric_rows' AND value >= ?1",
                [to_i64(excess)?],
            )
            .map_err(|_| ObservabilityError::storage())?;
        if changed != 1 {
            return Err(ObservabilityError::corrupt());
        }
    }
    Ok(())
}

fn decode_metric_row(row: &rusqlite::Row<'_>) -> Result<MetricRow, ObservabilityError> {
    let key_json: String = row.get(1).map_err(|_| ObservabilityError::corrupt())?;
    let key: MetricSeriesKey =
        serde_json::from_str(&key_json).map_err(|_| ObservabilityError::corrupt())?;
    key.validate().map_err(|_| ObservabilityError::corrupt())?;
    Ok(MetricRow {
        bucket_start_unix_millis: from_i64(row.get(0).map_err(|_| ObservabilityError::corrupt())?)?,
        key,
        observations: from_i64(row.get(2).map_err(|_| ObservabilityError::corrupt())?)?,
        latency_total_millis: from_i64(row.get(3).map_err(|_| ObservabilityError::corrupt())?)?,
        latency_max_millis: from_i64(row.get(4).map_err(|_| ObservabilityError::corrupt())?)?,
        recovered_items: from_i64(row.get(5).map_err(|_| ObservabilityError::corrupt())?)?,
        latest_used: from_i64(row.get(6).map_err(|_| ObservabilityError::corrupt())?)?,
        latest_limit: from_i64(row.get(7).map_err(|_| ObservabilityError::corrupt())?)?,
        maximum_used: from_i64(row.get(8).map_err(|_| ObservabilityError::corrupt())?)?,
    })
}

fn validate_bucket_window(
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

fn validate_limit(limit: u32, configured: u32) -> Result<(), ObservabilityError> {
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

fn parse_alert_status(value: &str) -> Result<AlertStatus, ObservabilityError> {
    match value {
        "firing" => Ok(AlertStatus::Firing),
        "resolved" => Ok(AlertStatus::Resolved),
        _ => Err(ObservabilityError::corrupt()),
    }
}

fn validate_alert_transition(transition: &AlertTransition) -> Result<(), ObservabilityError> {
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

const fn alert_status_key(status: AlertStatus) -> &'static str {
    match status {
        AlertStatus::Firing => "firing",
        AlertStatus::Resolved => "resolved",
    }
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

fn safe_integer(value: u64) -> Result<(), ObservabilityError> {
    if value > MAX_SAFE_INTEGER {
        return Err(ObservabilityError::invalid());
    }
    Ok(())
}

fn to_i64(value: u64) -> Result<i64, ObservabilityError> {
    i64::try_from(value).map_err(|_| ObservabilityError::invalid())
}

fn from_i64(value: i64) -> Result<u64, ObservabilityError> {
    u64::try_from(value).map_err(|_| ObservabilityError::corrupt())
}

fn sha256(bytes: &[u8]) -> String {
    format!("sha256:{:x}", Sha256::digest(bytes))
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

fn prepare_parent(path: &Path) -> Result<(), ObservabilityError> {
    let parent = path.parent().unwrap_or_else(|| Path::new("."));
    let existed = parent.exists();
    fs::create_dir_all(parent).map_err(|_| ObservabilityError::storage())?;
    if existed {
        Ok(())
    } else {
        protect_path(parent, 0o700)
    }
}

fn protect_database(path: &Path) -> Result<(), ObservabilityError> {
    protect_path(path, 0o600)
}

#[cfg(unix)]
fn protect_path(path: &Path, mode: u32) -> Result<(), ObservabilityError> {
    use std::os::unix::fs::PermissionsExt;

    fs::set_permissions(path, fs::Permissions::from_mode(mode))
        .map_err(|_| ObservabilityError::storage())
}

#[cfg(not(unix))]
fn protect_path(_path: &Path, _mode: u32) -> Result<(), ObservabilityError> {
    Ok(())
}
