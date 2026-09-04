// SPDX-License-Identifier: Apache-2.0

//! Read-only export of durable React-versus-DelegatedBatch performance facts.

use std::collections::{BTreeMap, BTreeSet};
use std::fmt;
use std::path::Path;

use rusqlite::{Connection, OpenFlags};
use serde::{Deserialize, Serialize};
use sha2::{Digest as _, Sha256};
use winwincode_domain::{ExecutionJobId, Instant, ModelExchangeId, RequestId, Sha256Digest};
use winwincode_execution_port::performance_comparison::{
    PerformanceV0Comparison, PerformanceV0ComparisonError, PerformanceV0ModelCallEvidence,
    PerformanceV0ModelKind, PerformanceV0RunEvidence, summarize_performance_v0,
};
use winwincode_execution_port::performance_evaluation::PerformanceArmMeasurementV1;
use winwincode_execution_port::runtime_trace_outbox::{ExecutionMode, ObserverMode};

use crate::performance::{StoredPerformanceProjection, elapsed_millis};
use crate::store::DATABASE_FILE;

const MAX_SAFE_METRIC: i64 = 9_007_199_254_740_991;

/// Secret-safe evidence exported from one Worker Codex store.
///
/// The run projection contains only metrics backed by both React and
/// delegated durable facts. Delegated workspace Patch and Validation counters
/// are omitted until that execution boundary has an authoritative settlement.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ProductionPerformanceV0Evidence {
    pub runs: Vec<PerformanceV0RunEvidence>,
    pub model_calls: Vec<PerformanceV0ModelCallEvidence>,
}

impl ProductionPerformanceV0Evidence {
    /// Reduces this exact export into the two V0 comparison arms.
    ///
    /// # Errors
    ///
    /// Returns an error when durable run and model-call facts do not reconcile.
    pub fn summarize(&self) -> Result<PerformanceV0Comparison, PerformanceV0ComparisonError> {
        summarize_performance_v0(&self.runs, &self.model_calls)
    }
}

/// Durable Worker link from one hashed metric row to its CP model request.
#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ProductionPerformanceModelCallAuthority {
    model_call_digest: Sha256Digest,
    request_id: RequestId,
    initial_model_exchange_id: ModelExchangeId,
}

impl ProductionPerformanceModelCallAuthority {
    #[must_use]
    pub const fn model_call_digest(&self) -> &Sha256Digest {
        &self.model_call_digest
    }

    #[must_use]
    pub const fn request_id(&self) -> &RequestId {
        &self.request_id
    }

    #[must_use]
    pub const fn initial_model_exchange_id(&self) -> &ModelExchangeId {
        &self.initial_model_exchange_id
    }
}

/// One exact terminal arm projected from a single Worker database snapshot.
#[derive(Clone, Debug, PartialEq)]
pub struct ProductionPerformanceEvaluationArm {
    measurement: PerformanceArmMeasurementV1,
    candidate_artifact: winwincode_execution_port::generated::ArtifactReference,
    candidate_artifact_ack_revision: u64,
    primary_model_calls: Vec<ProductionPerformanceModelCallAuthority>,
    worker_ledger_snapshot_digest: Sha256Digest,
}

impl ProductionPerformanceEvaluationArm {
    #[must_use]
    pub const fn measurement(&self) -> &PerformanceArmMeasurementV1 {
        &self.measurement
    }

    #[must_use]
    pub const fn candidate_artifact(
        &self,
    ) -> &winwincode_execution_port::generated::ArtifactReference {
        &self.candidate_artifact
    }

    #[must_use]
    pub const fn candidate_artifact_ack_revision(&self) -> u64 {
        self.candidate_artifact_ack_revision
    }

    #[must_use]
    pub fn primary_model_calls(&self) -> &[ProductionPerformanceModelCallAuthority] {
        &self.primary_model_calls
    }

    /// Digest of every Worker fact read by the single export transaction.
    #[must_use]
    pub const fn worker_ledger_snapshot_digest(&self) -> &Sha256Digest {
        &self.worker_ledger_snapshot_digest
    }
}

/// Read-only export failure with no database path or private identity content.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProductionPerformanceEvidenceError {
    Unavailable,
    Corrupt,
    ObserverAuthorityUnavailable,
    Inconsistent(PerformanceV0ComparisonError),
}

impl fmt::Display for ProductionPerformanceEvidenceError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(match self {
            Self::Unavailable => "performance evidence store is unavailable",
            Self::Corrupt => "performance evidence store is invalid",
            Self::ObserverAuthorityUnavailable => {
                "Observer route authority is not available for performance evaluation"
            }
            Self::Inconsistent(_) => "performance evidence facts do not reconcile",
        })
    }
}

impl std::error::Error for ProductionPerformanceEvidenceError {}

impl From<PerformanceV0ComparisonError> for ProductionPerformanceEvidenceError {
    fn from(error: PerformanceV0ComparisonError) -> Self {
        Self::Inconsistent(error)
    }
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct StoredRunTiming {
    #[serde(default)]
    job: Option<StoredRunJob>,
    last_activity_at: Instant,
    #[serde(default)]
    final_candidate_freeze: Option<StoredElapsedFact>,
    #[serde(default)]
    delegated_stop: Option<StoredElapsedFact>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct StoredRunJob {
    job_id: ExecutionJobId,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct StoredElapsedFact {
    counters: StoredElapsedCounters,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct StoredElapsedCounters {
    elapsed_millis: i64,
}

struct ExportedRun {
    raw_run_key: String,
    job_id: Option<ExecutionJobId>,
    evidence: PerformanceV0RunEvidence,
}

/// Opens an existing `AdapterStore` database in `SQLite` read-only mode and
/// exports retained terminal performance facts.
///
/// Raw run and model-call identities are converted to domain-separated
/// SHA-256 digests. Reserved but not retained terminal projections are omitted.
/// The exported runtime spans the first durable operation through the final
/// durable activity, rather than copying the last turn's duration.
///
/// # Errors
///
/// Returns an error if the store is unavailable, malformed, or its retained
/// terminal report disagrees with the model-call ledger.
pub fn export_performance_v0_evidence(
    data_directory: &Path,
) -> Result<ProductionPerformanceV0Evidence, ProductionPerformanceEvidenceError> {
    let mut connection = Connection::open_with_flags(
        data_directory.join(DATABASE_FILE),
        OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX,
    )
    .map_err(|_| ProductionPerformanceEvidenceError::Unavailable)?;
    connection
        .execute_batch("PRAGMA query_only=ON;")
        .map_err(|_| ProductionPerformanceEvidenceError::Unavailable)?;

    let transaction = connection
        .transaction()
        .map_err(|_| ProductionPerformanceEvidenceError::Unavailable)?;
    let runs = read_retained_runs(&transaction)?;
    let raw_run_keys = runs
        .iter()
        .map(|run| (run.raw_run_key.clone(), run.evidence.run_id.clone()))
        .collect::<BTreeMap<_, _>>();
    let mut model_calls = read_primary_model_calls(&transaction, &raw_run_keys)?;
    model_calls.extend(read_observer_model_calls(&transaction, &raw_run_keys)?);
    let evidence = ProductionPerformanceV0Evidence {
        runs: runs.into_iter().map(|run| run.evidence).collect(),
        model_calls,
    };
    evidence.summarize()?;
    transaction
        .commit()
        .map_err(|_| ProductionPerformanceEvidenceError::Unavailable)?;
    Ok(evidence)
}

/// Projects one terminal, Observer-off arm and its CP join keys from a single
/// read-only Worker database transaction.
///
/// # Errors
///
/// Rejects a missing/ambiguous run or Candidate, a different Job, incomplete
/// model links, and Observer-on runs whose provider-attempt identity is not yet
/// retained by the Worker ledger.
pub fn export_performance_evaluation_arm(
    data_directory: &Path,
    expected_run_id: &Sha256Digest,
    expected_job_id: &ExecutionJobId,
) -> Result<ProductionPerformanceEvaluationArm, ProductionPerformanceEvidenceError> {
    let mut connection = Connection::open_with_flags(
        data_directory.join(DATABASE_FILE),
        OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX,
    )
    .map_err(|_| ProductionPerformanceEvidenceError::Unavailable)?;
    connection
        .execute_batch("PRAGMA query_only=ON;")
        .map_err(|_| ProductionPerformanceEvidenceError::Unavailable)?;
    let transaction = connection
        .transaction()
        .map_err(|_| ProductionPerformanceEvidenceError::Unavailable)?;
    let runs = read_retained_runs(&transaction)?;
    let raw_run_keys = runs
        .iter()
        .map(|run| (run.raw_run_key.clone(), run.evidence.run_id.clone()))
        .collect::<BTreeMap<_, _>>();
    let mut all_calls = read_primary_model_calls(&transaction, &raw_run_keys)?;
    all_calls.extend(read_observer_model_calls(&transaction, &raw_run_keys)?);
    let mut matching = runs
        .into_iter()
        .filter(|run| run.evidence.run_id == *expected_run_id);
    let run = matching
        .next()
        .ok_or(ProductionPerformanceEvidenceError::Corrupt)?;
    if matching.next().is_some() || run.job_id.as_ref() != Some(expected_job_id) {
        return Err(ProductionPerformanceEvidenceError::Corrupt);
    }
    let calls = all_calls
        .into_iter()
        .filter(|call| call.run_id == *expected_run_id)
        .collect::<Vec<_>>();
    if run.evidence.observer_mode != ObserverMode::Off
        || calls
            .iter()
            .any(|call| call.model_kind == PerformanceV0ModelKind::Observer)
    {
        return Err(ProductionPerformanceEvidenceError::ObserverAuthorityUnavailable);
    }
    let primary_model_calls =
        read_primary_model_call_authorities(&transaction, &run.raw_run_key, &calls)?;
    let candidate = crate::candidate_artifact_outbox::read_accepted_candidate_artifact(
        &transaction,
        expected_job_id,
    )
    .map_err(|error| match error {
        crate::store::AdapterStoreError::Unavailable => {
            ProductionPerformanceEvidenceError::Unavailable
        }
        crate::store::AdapterStoreError::Corrupt | crate::store::AdapterStoreError::Conflict => {
            ProductionPerformanceEvidenceError::Corrupt
        }
    })?
    .ok_or(ProductionPerformanceEvidenceError::Corrupt)?;
    let measurement = PerformanceArmMeasurementV1::from_v0(run.evidence, calls)
        .map_err(|_| ProductionPerformanceEvidenceError::Corrupt)?;
    let worker_ledger_snapshot_digest = evaluation_arm_snapshot_digest(
        &measurement,
        &candidate.artifact,
        candidate.ack_revision,
        &primary_model_calls,
    )?;
    let arm = ProductionPerformanceEvaluationArm {
        measurement,
        candidate_artifact: candidate.artifact,
        candidate_artifact_ack_revision: candidate.ack_revision,
        primary_model_calls,
        worker_ledger_snapshot_digest,
    };
    transaction
        .commit()
        .map_err(|_| ProductionPerformanceEvidenceError::Unavailable)?;
    Ok(arm)
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct EvaluationArmSnapshotFacts<'facts> {
    measurement: &'facts PerformanceArmMeasurementV1,
    candidate_artifact: &'facts winwincode_execution_port::generated::ArtifactReference,
    candidate_artifact_ack_revision: u64,
    primary_model_calls: &'facts [ProductionPerformanceModelCallAuthority],
}

fn evaluation_arm_snapshot_digest(
    measurement: &PerformanceArmMeasurementV1,
    candidate_artifact: &winwincode_execution_port::generated::ArtifactReference,
    candidate_artifact_ack_revision: u64,
    primary_model_calls: &[ProductionPerformanceModelCallAuthority],
) -> Result<Sha256Digest, ProductionPerformanceEvidenceError> {
    let bytes = serde_json::to_vec(&EvaluationArmSnapshotFacts {
        measurement,
        candidate_artifact,
        candidate_artifact_ack_revision,
        primary_model_calls,
    })
    .map_err(|_| ProductionPerformanceEvidenceError::Corrupt)?;
    let mut digest = Sha256::new();
    digest.update(b"winwincode.performance-worker-ledger-snapshot.v1");
    digest.update(bytes);
    Ok(Sha256Digest(format!("sha256:{:x}", digest.finalize())))
}

fn read_primary_model_call_authorities(
    connection: &Connection,
    run_key: &str,
    calls: &[PerformanceV0ModelCallEvidence],
) -> Result<Vec<ProductionPerformanceModelCallAuthority>, ProductionPerformanceEvidenceError> {
    let expected = calls
        .iter()
        .filter(|call| call.model_kind == PerformanceV0ModelKind::Primary)
        .map(|call| call.model_call_id.0.as_str())
        .collect::<BTreeSet<_>>();
    let mut statement = connection
        .prepare(
            "SELECT model_call_id, model_exchange_id, provider_final
             FROM model_call_ledger WHERE run_key = ?1 ORDER BY ordinal",
        )
        .map_err(|_| ProductionPerformanceEvidenceError::Unavailable)?;
    let rows = statement
        .query_map([run_key], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, Option<String>>(1)?,
                row.get::<_, i64>(2)?,
            ))
        })
        .map_err(|_| ProductionPerformanceEvidenceError::Unavailable)?;
    let mut projected = Vec::new();
    let mut actual = BTreeSet::new();
    for row in rows {
        let (request_id, model_exchange_id, provider_final) =
            row.map_err(|_| ProductionPerformanceEvidenceError::Unavailable)?;
        let model_call_digest = evidence_digest(
            b"winwincode.performance-v0-model-call.v1",
            &[run_key, &request_id],
        );
        if provider_final != 1 || !actual.insert(model_call_digest.0.clone()) {
            return Err(ProductionPerformanceEvidenceError::Corrupt);
        }
        projected.push(ProductionPerformanceModelCallAuthority {
            model_call_digest,
            request_id: RequestId(request_id),
            initial_model_exchange_id: ModelExchangeId(
                model_exchange_id.ok_or(ProductionPerformanceEvidenceError::Corrupt)?,
            ),
        });
    }
    if actual.iter().map(String::as_str).collect::<BTreeSet<_>>() != expected {
        return Err(ProductionPerformanceEvidenceError::Corrupt);
    }
    Ok(projected)
}

fn read_retained_runs(
    connection: &Connection,
) -> Result<Vec<ExportedRun>, ProductionPerformanceEvidenceError> {
    let mut statement = connection
        .prepare(
            "SELECT performance_run.run_key,
                    performance_run.execution_mode,
                    performance_run.observer_mode,
                    performance_projection.record_json,
                    codex_run.record_json,
                    (SELECT MIN(started_at) FROM performance_operation
                     WHERE performance_operation.run_key = performance_run.run_key),
                    (SELECT MAX(completed_at) FROM performance_operation
                     WHERE performance_operation.run_key = performance_run.run_key)
             FROM performance_run
             JOIN performance_projection
               ON performance_projection.run_key = performance_run.run_key
             JOIN codex_run ON codex_run.run_key = performance_run.run_key
             ORDER BY performance_run.run_key",
        )
        .map_err(|_| ProductionPerformanceEvidenceError::Unavailable)?;
    let rows = statement
        .query_map([], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, String>(2)?,
                row.get::<_, Vec<u8>>(3)?,
                row.get::<_, Vec<u8>>(4)?,
                row.get::<_, Option<String>>(5)?,
                row.get::<_, Option<String>>(6)?,
            ))
        })
        .map_err(|_| ProductionPerformanceEvidenceError::Unavailable)?;
    let mut exported = Vec::new();
    for row in rows {
        let (
            run_key,
            execution_mode,
            observer_mode,
            projection,
            run,
            started_at,
            last_completed_at,
        ) = row.map_err(|_| ProductionPerformanceEvidenceError::Unavailable)?;
        let projection: StoredPerformanceProjection = serde_json::from_slice(&projection)
            .map_err(|_| ProductionPerformanceEvidenceError::Corrupt)?;
        if !projection.retained {
            continue;
        }
        validate_projection(&projection, &execution_mode, &observer_mode)?;
        let timing: StoredRunTiming = serde_json::from_slice(&run)
            .map_err(|_| ProductionPerformanceEvidenceError::Corrupt)?;
        let mut report = projection.report;
        if let Some(started_at) = started_at {
            let started_at = Instant(started_at);
            report.total_runtime_ms = report.total_runtime_ms.max(
                elapsed_millis(&started_at, &timing.last_activity_at)
                    .ok_or(ProductionPerformanceEvidenceError::Corrupt)?,
            );
            if let Some(last_completed_at) = last_completed_at {
                report.total_runtime_ms = report.total_runtime_ms.max(
                    elapsed_millis(&started_at, &Instant(last_completed_at))
                        .ok_or(ProductionPerformanceEvidenceError::Corrupt)?,
                );
            }
        }
        report.total_runtime_ms = report
            .total_runtime_ms
            .max(
                timing
                    .final_candidate_freeze
                    .as_ref()
                    .map_or(0, |freeze| freeze.counters.elapsed_millis),
            )
            .max(
                timing
                    .delegated_stop
                    .as_ref()
                    .map_or(0, |stop| stop.counters.elapsed_millis),
            );
        validate_stored_elapsed(&timing)?;
        exported.push(ExportedRun {
            job_id: timing.job.map(|job| job.job_id),
            evidence: PerformanceV0RunEvidence {
                run_id: evidence_digest(b"winwincode.performance-v0-run.v1", &[&run_key]),
                execution_mode: report.execution_mode,
                observer_mode: report.observer_mode,
                primary_model_call_count: report.primary_model_call_count,
                primary_model_input_tokens: report.primary_model_input_tokens,
                primary_model_cached_tokens: report.primary_model_cached_tokens,
                primary_model_output_tokens: report.primary_model_output_tokens,
                primary_model_wait_ms: report.primary_model_wait_ms,
                observer_call_count: report.observer_call_count,
                observer_wait_ms: report.observer_wait_ms,
                total_runtime_ms: report.total_runtime_ms,
            },
            raw_run_key: run_key,
        });
    }
    Ok(exported)
}

fn validate_projection(
    projection: &StoredPerformanceProjection,
    execution_mode: &str,
    observer_mode: &str,
) -> Result<(), ProductionPerformanceEvidenceError> {
    let configured_execution_mode = ExecutionMode::from_config(execution_mode)
        .ok_or(ProductionPerformanceEvidenceError::Corrupt)?;
    let configured_observer_mode = ObserverMode::from_config(observer_mode)
        .ok_or(ProductionPerformanceEvidenceError::Corrupt)?;
    let report_bytes = serde_json::to_vec(&projection.report)
        .map_err(|_| ProductionPerformanceEvidenceError::Corrupt)?;
    let report_digest = Sha256Digest(format!("sha256:{:x}", Sha256::digest(report_bytes)));
    if !(1..=MAX_SAFE_METRIC).contains(&projection.sequence.0)
        || projection.report_digest != report_digest
        || projection.report.execution_mode != configured_execution_mode
        || projection.report.observer_mode != configured_observer_mode
    {
        return Err(ProductionPerformanceEvidenceError::Corrupt);
    }
    Ok(())
}

fn validate_stored_elapsed(
    timing: &StoredRunTiming,
) -> Result<(), ProductionPerformanceEvidenceError> {
    for elapsed in [
        timing
            .final_candidate_freeze
            .as_ref()
            .map(|freeze| freeze.counters.elapsed_millis),
        timing
            .delegated_stop
            .as_ref()
            .map(|stop| stop.counters.elapsed_millis),
    ]
    .into_iter()
    .flatten()
    {
        if !(0..=MAX_SAFE_METRIC).contains(&elapsed) {
            return Err(ProductionPerformanceEvidenceError::Corrupt);
        }
    }
    Ok(())
}

fn read_primary_model_calls(
    connection: &Connection,
    run_ids: &BTreeMap<String, Sha256Digest>,
) -> Result<Vec<PerformanceV0ModelCallEvidence>, ProductionPerformanceEvidenceError> {
    let mut statement = connection
        .prepare(
            "SELECT model_call_ledger.run_key,
                    model_call_ledger.model_call_id,
                    model_call_ledger.provider_final,
                    performance_operation.operation_id,
                    performance_operation.completed,
                    performance_operation.duration_millis,
                    performance_operation.input_tokens,
                    performance_operation.cached_tokens,
                    performance_operation.output_tokens,
                    performance_operation.actual_cost_microunits
             FROM model_call_ledger
             LEFT JOIN performance_operation
               ON performance_operation.run_key = model_call_ledger.run_key
              AND performance_operation.operation_kind = 'primary_model'
              AND performance_operation.operation_id = model_call_ledger.model_call_id
             ORDER BY model_call_ledger.run_key, model_call_ledger.model_call_id",
        )
        .map_err(|_| ProductionPerformanceEvidenceError::Unavailable)?;
    let rows = statement
        .query_map([], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, i64>(2)?,
                row.get::<_, Option<String>>(3)?,
                row.get::<_, Option<i64>>(4)?,
                row.get::<_, Option<i64>>(5)?,
                row.get::<_, Option<i64>>(6)?,
                row.get::<_, Option<i64>>(7)?,
                row.get::<_, Option<i64>>(8)?,
                row.get::<_, Option<i64>>(9)?,
            ))
        })
        .map_err(|_| ProductionPerformanceEvidenceError::Unavailable)?;
    let mut exported = Vec::new();
    for row in rows {
        let (
            run_key,
            model_call_id,
            provider_final,
            operation_id,
            completed,
            elapsed_millis,
            input_tokens,
            cached_tokens,
            output_tokens,
            actual_cost_microunits,
        ) = row.map_err(|_| ProductionPerformanceEvidenceError::Unavailable)?;
        let Some(run_id) = run_ids.get(&run_key) else {
            continue;
        };
        let operation_id = operation_id.ok_or(ProductionPerformanceEvidenceError::Corrupt)?;
        let completed = completed.ok_or(ProductionPerformanceEvidenceError::Corrupt)?;
        if operation_id != model_call_id
            || !matches!(completed, 0 | 1)
            || !matches!(provider_final, 0 | 1)
            || completed != provider_final
        {
            return Err(ProductionPerformanceEvidenceError::Corrupt);
        }
        exported.push(PerformanceV0ModelCallEvidence {
            run_id: run_id.clone(),
            model_call_id: evidence_digest(
                b"winwincode.performance-v0-model-call.v1",
                &[&run_key, &model_call_id],
            ),
            model_kind: PerformanceV0ModelKind::Primary,
            completed: completed == 1,
            input_tokens: input_tokens.ok_or(ProductionPerformanceEvidenceError::Corrupt)?,
            cached_tokens: cached_tokens.ok_or(ProductionPerformanceEvidenceError::Corrupt)?,
            output_tokens: output_tokens.ok_or(ProductionPerformanceEvidenceError::Corrupt)?,
            elapsed_millis: elapsed_millis.ok_or(ProductionPerformanceEvidenceError::Corrupt)?,
            actual_cost_microunits,
        });
    }
    Ok(exported)
}

fn read_observer_model_calls(
    connection: &Connection,
    run_ids: &BTreeMap<String, Sha256Digest>,
) -> Result<Vec<PerformanceV0ModelCallEvidence>, ProductionPerformanceEvidenceError> {
    let mut statement = connection
        .prepare(
            "SELECT run_key, operation_id, completed, duration_millis,
                    input_tokens, cached_tokens, output_tokens,
                    actual_cost_microunits
             FROM performance_operation
             WHERE operation_kind = 'observer'
             ORDER BY run_key, operation_id",
        )
        .map_err(|_| ProductionPerformanceEvidenceError::Unavailable)?;
    let rows = statement
        .query_map([], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, i64>(2)?,
                row.get::<_, i64>(3)?,
                row.get::<_, i64>(4)?,
                row.get::<_, i64>(5)?,
                row.get::<_, i64>(6)?,
                row.get::<_, Option<i64>>(7)?,
            ))
        })
        .map_err(|_| ProductionPerformanceEvidenceError::Unavailable)?;
    let mut exported = Vec::new();
    for row in rows {
        let (
            run_key,
            operation_id,
            completed,
            elapsed_millis,
            input_tokens,
            cached_tokens,
            output_tokens,
            actual_cost_microunits,
        ) = row.map_err(|_| ProductionPerformanceEvidenceError::Unavailable)?;
        let Some(run_id) = run_ids.get(&run_key) else {
            continue;
        };
        if !matches!(completed, 0 | 1) {
            return Err(ProductionPerformanceEvidenceError::Corrupt);
        }
        exported.push(PerformanceV0ModelCallEvidence {
            run_id: run_id.clone(),
            model_call_id: evidence_digest(
                b"winwincode.performance-v0-observer-call.v1",
                &[&run_key, &operation_id],
            ),
            model_kind: PerformanceV0ModelKind::Observer,
            completed: completed == 1,
            input_tokens,
            cached_tokens,
            output_tokens,
            elapsed_millis,
            actual_cost_microunits,
        });
    }
    Ok(exported)
}

fn evidence_digest(domain: &[u8], parts: &[&str]) -> Sha256Digest {
    let mut digest = Sha256::new();
    digest.update(domain);
    for part in parts {
        digest.update(u64::try_from(part.len()).unwrap_or(u64::MAX).to_be_bytes());
        digest.update(part.as_bytes());
    }
    Sha256Digest(format!("sha256:{:x}", digest.finalize()))
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::path::{Path, PathBuf};

    use rusqlite::{Connection, params};
    use winwincode_domain::{ExecutionEventId, ExecutionSequence};
    use winwincode_execution_port::runtime_trace_outbox::PerformanceBaselineReport;

    use super::*;

    fn test_root() -> PathBuf {
        let unique = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("system clock")
            .as_nanos();
        std::env::temp_dir().join(format!(
            "winwincode-performance-export-{}-{unique}",
            std::process::id()
        ))
    }

    fn report(mode: ExecutionMode) -> PerformanceBaselineReport {
        let has_observer = mode == ExecutionMode::DelegatedPatch;
        PerformanceBaselineReport {
            execution_mode: mode,
            observer_mode: if has_observer {
                ObserverMode::Always
            } else {
                ObserverMode::Off
            },
            primary_model_call_count: 1,
            primary_model_input_tokens: 20,
            primary_model_cached_tokens: 5,
            primary_model_output_tokens: 10,
            primary_model_wait_ms: 400,
            tool_call_count: 0,
            patch_call_count: 0,
            patch_apply_ms: 0,
            files_changed: 0,
            validation_ms: 0,
            observer_call_count: i64::from(has_observer),
            observer_wait_ms: i64::from(has_observer) * 100,
            repair_rounds: 0,
            turn_count: 2,
            total_runtime_ms: 700,
        }
    }

    fn insert_run(connection: &Connection, run_key: &str, mode: ExecutionMode) {
        let report = report(mode);
        let observer_mode = report.observer_mode;
        let report_bytes = serde_json::to_vec(&report).expect("encode report");
        let projection = StoredPerformanceProjection {
            event_id: ExecutionEventId(format!("xevt_{run_key}")),
            sequence: ExecutionSequence(1),
            report,
            report_digest: Sha256Digest(format!("sha256:{:x}", Sha256::digest(report_bytes))),
            retained: true,
        };
        connection
            .execute(
                "INSERT INTO performance_run VALUES (?1, ?2, ?3)",
                params![run_key, mode.as_config(), observer_mode.as_config()],
            )
            .expect("insert performance run");
        connection
            .execute(
                "INSERT INTO performance_projection VALUES (?1, ?2)",
                params![
                    run_key,
                    serde_json::to_vec(&projection).expect("encode projection")
                ],
            )
            .expect("insert projection");
        connection
            .execute(
                "INSERT INTO codex_run VALUES (?1, ?2)",
                params![
                    run_key,
                    serde_json::to_vec(&serde_json::json!({
                        "lastActivityAt": "2030-01-01T00:00:03.000Z",
                        "finalCandidateFreeze": {
                            "counters": { "elapsedMillis": 5_000 }
                        }
                    }))
                    .expect("encode timing")
                ],
            )
            .expect("insert run timing");
        connection
            .execute(
                "INSERT INTO model_call_ledger VALUES (?1, 'model-call', 1)",
                params![run_key],
            )
            .expect("insert model-call ledger");
        connection
            .execute(
                "INSERT INTO performance_operation VALUES (
                   ?1, 'primary_model', 'model-call',
                   '2030-01-01T00:00:00.000Z', '2030-01-01T00:00:04.000Z',
                   1, 400, 20, 5, 10, 9
                 )",
                params![run_key],
            )
            .expect("insert performance operation");
        if mode == ExecutionMode::DelegatedPatch {
            connection
                .execute(
                    "INSERT INTO performance_operation VALUES (
                       ?1, 'observer', 'observer-call',
                       '2030-01-01T00:00:04.000Z', '2030-01-01T00:00:05.000Z',
                       1, 100, 7, 0, 3, 4
                     )",
                    params![run_key],
                )
                .expect("insert Observer operation");
        }
    }

    fn create_fixture(root: &Path) {
        fs::create_dir_all(root).expect("create fixture root");
        let connection = Connection::open(root.join(DATABASE_FILE)).expect("create fixture DB");
        connection
            .execute_batch(
                "CREATE TABLE performance_run (
                   run_key TEXT PRIMARY KEY, execution_mode TEXT, observer_mode TEXT
                 );
                 CREATE TABLE performance_projection (
                   run_key TEXT PRIMARY KEY, record_json BLOB
                 );
                 CREATE TABLE codex_run (run_key TEXT PRIMARY KEY, record_json BLOB);
                 CREATE TABLE model_call_ledger (
                   run_key TEXT, model_call_id TEXT, provider_final INTEGER
                 );
                 CREATE TABLE performance_operation (
                   run_key TEXT, operation_kind TEXT, operation_id TEXT,
                   started_at TEXT, completed_at TEXT, completed INTEGER,
                   duration_millis INTEGER,
                   input_tokens INTEGER, cached_tokens INTEGER, output_tokens INTEGER,
                   actual_cost_microunits INTEGER
                 );",
            )
            .expect("create durable fact tables");
        insert_run(
            &connection,
            "shadow-run",
            ExecutionMode::DelegatedPatchShadow,
        );
        insert_run(&connection, "structured-run", ExecutionMode::DelegatedPatch);
    }

    #[test]
    fn read_only_export_uses_whole_run_and_replay_does_not_inflate_comparison() {
        let root = test_root();
        create_fixture(&root);

        let first = export_performance_v0_evidence(&root).expect("export durable evidence");
        let second = export_performance_v0_evidence(&root).expect("re-export durable evidence");
        assert_eq!(first, second);
        let serialized = serde_json::to_string(&first).expect("serialize evidence");
        assert!(!serialized.contains("shadow-run"));
        assert!(!serialized.contains("structured-run"));
        assert!(!serialized.contains("model-call"));
        assert!(!serialized.contains("observer-call"));
        assert!(!serialized.contains("patchCallCount"));
        assert!(!serialized.contains("validationMs"));
        assert!(first.runs.iter().all(|run| run.total_runtime_ms == 5_000));
        assert_eq!(
            first
                .summarize()
                .expect("summarize first")
                .react
                .sample_count,
            1
        );
        let first_comparison = first.summarize().expect("summarize Observer totals");
        assert_eq!(first_comparison.react.total_tokens, 35);
        assert_eq!(first_comparison.react.settled_cost_microunits, 9);
        assert_eq!(first_comparison.structured.observer_model_call_count, 1);
        assert_eq!(
            first_comparison.structured.total_observer_model_wait_ms,
            100
        );
        assert_eq!(first_comparison.structured.total_tokens, 45);
        assert_eq!(first_comparison.structured.settled_cost_microunits, 13);
        assert_eq!(
            first
                .summarize()
                .expect("summarize first")
                .structured
                .sample_count,
            1
        );

        let combined = ProductionPerformanceV0Evidence {
            runs: first.runs.into_iter().chain(second.runs).collect(),
            model_calls: first
                .model_calls
                .into_iter()
                .chain(second.model_calls)
                .collect(),
        };
        let comparison = combined.summarize().expect("deduplicate two snapshots");
        assert_eq!(comparison.react.total_runtime_ms, 5_000);
        assert_eq!(comparison.structured.total_runtime_ms, 5_000);
        assert_eq!(comparison.react.duplicate_run_write_count, 1);
        assert_eq!(comparison.structured.duplicate_model_call_write_count, 2);
        assert_eq!(
            comparison.structured.duplicate_settled_charge_microunits,
            13
        );

        fs::remove_dir_all(root).expect("remove fixture");
    }
}
