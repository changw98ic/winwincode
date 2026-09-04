// SPDX-License-Identifier: Apache-2.0

//! Secret-safe performance accounting for one exact Codex run.

use serde::{Deserialize, Serialize};
use winwincode_domain::{ExecutionEventId, ExecutionSequence, Instant, Sha256Digest};
use winwincode_execution_port::runtime_trace_outbox::PerformanceBaselineReport;

/// Stable aggregate category. The string value is persisted in the private
/// Worker store and must remain one-to-one with this enum.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum PerformanceOperationKind {
    PrimaryModel,
    Tool,
    Patch,
    Validation,
    Turn,
}

impl PerformanceOperationKind {
    pub(crate) const fn as_str(self) -> &'static str {
        match self {
            Self::PrimaryModel => "primary_model",
            Self::Tool => "tool",
            Self::Patch => "patch",
            Self::Validation => "validation",
            Self::Turn => "turn",
        }
    }
}

/// Bounded values recorded when one stable operation completes.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub(crate) struct PerformanceOperationCompletion {
    pub(crate) duration_millis: Option<i64>,
    pub(crate) input_tokens: i64,
    pub(crate) cached_tokens: i64,
    pub(crate) output_tokens: i64,
}

/// Exact terminal model usage decoded from a provider-neutral `ModelPort` frame.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub(crate) struct PrimaryModelUsage {
    pub(crate) input: i64,
    pub(crate) cached: i64,
    pub(crate) output: i64,
}

/// Durable report projection reservation used to replay the same event after
/// a process stop.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub(crate) struct StoredPerformanceProjection {
    pub(crate) event_id: ExecutionEventId,
    pub(crate) sequence: ExecutionSequence,
    pub(crate) report: PerformanceBaselineReport,
    pub(crate) report_digest: Sha256Digest,
    pub(crate) retained: bool,
}

/// Returns a bounded millisecond value or `None` for a malformed/backwards
/// interval. Callers then retain the operation with unknown duration instead
/// of inventing timing data.
pub(crate) fn elapsed_millis(started_at: &Instant, completed_at: &Instant) -> Option<i64> {
    use time::{OffsetDateTime, format_description::well_known::Rfc3339};

    let start = OffsetDateTime::parse(&started_at.0, &Rfc3339).ok()?;
    let completed = OffsetDateTime::parse(&completed_at.0, &Rfc3339).ok()?;
    let millis = (completed - start).whole_milliseconds();
    i64::try_from(millis).ok().filter(|value| *value >= 0)
}

pub(crate) fn duration_millis(duration: std::time::Duration) -> i64 {
    i64::try_from(duration.as_millis()).unwrap_or(i64::MAX)
}
