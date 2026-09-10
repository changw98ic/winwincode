// SPDX-License-Identifier: Apache-2.0
//! Strict one-time conversion from the canonical Delivery aggregate.
use crate::domain::Delivery;
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use winwincode_domain::{WorkContract, WorkItem, WorkRun};

pub const WORKRUN_MIGRATION_SCHEMA_VERSION: &str = "winwincode.delivery-canonical-to-workrun.v1";
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WorkRunMigrationOutcome {
    Applied {
        source_key: String,
        canonical_snapshot: Vec<u8>,
    },
    AlreadyConsumed {
        source_key: String,
        canonical_snapshot: Vec<u8>,
    },
}
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WorkRunMigrationError {
    InvalidInput(String),
    CorruptState(String),
    Transaction(String),
}
impl std::fmt::Display for WorkRunMigrationError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "WorkRun migration error: {self:?}")
    }
}
impl std::error::Error for WorkRunMigrationError {}

/// Convert one complete frozen source without granting execution authority.
///
/// # Errors
/// Returns an error for malformed source data or an unrepresentable target.
#[allow(clippy::too_many_lines)]
pub fn convert_canonical_delivery(
    input: &[u8],
) -> Result<(String, Vec<u8>), WorkRunMigrationError> {
    let delivery = Delivery::decode_json(input)
        .map_err(|e| WorkRunMigrationError::InvalidInput(e.to_string()))?;
    let snap = delivery.snapshot();
    let source_key = format!(
        "{WORKRUN_MIGRATION_SCHEMA_VERSION}:{}:{}",
        snap.id.0, snap.revision
    );
    let mut run_values = Vec::new();
    let mut historical = Vec::new();
    for stage in &snap.stage_runs {
        let Some(task_id) = &stage.delivery_task_id else {
            historical.push(
                serde_json::to_value(stage)
                    .map_err(|e| WorkRunMigrationError::InvalidInput(e.to_string()))?,
            );
            continue;
        };
        // ponytail: at most 1000 bindings; index by stage if the import limit grows.
        // The bounded source graph may contain multiple historical bindings.
        // Only a unique binding can become a frozen WorkRun identity.
        let mut bindings = snap
            .session_bindings
            .iter()
            .filter(|b| b.stage_run_id == stage.id);
        let Some(binding) = bindings.next().filter(|_| bindings.next().is_none()) else {
            historical.push(
                serde_json::to_value(stage)
                    .map_err(|e| WorkRunMigrationError::InvalidInput(e.to_string()))?,
            );
            continue;
        };
        let identity_valid = binding
            .worker_id
            .as_ref()
            .is_some_and(|v| valid_id("wrk_", &v.0))
            && binding
                .worker_instance_id
                .as_ref()
                .is_some_and(|v| valid_id("wki_", &v.0))
            && binding
                .worker_session_id
                .as_ref()
                .is_some_and(|v| valid_id("wsn_", &v.0))
            && binding
                .lease_id
                .as_ref()
                .is_some_and(|v| valid_id("lse_", &v.0))
            && valid_id("job_", &binding.execution_job_id.0)
            && valid_id("psn_", &binding.product_session_id.0)
            && binding
                .codex_thread_id
                .as_ref()
                .is_some_and(|v| valid_id("cdx_", &v.0))
            && binding
                .fencing_token
                .as_ref()
                .is_some_and(|v| v.0.parse::<u64>().is_ok_and(|n| n > 0))
            && stage.attempt <= 1000;
        if !identity_valid {
            historical.push(
                serde_json::to_value(stage)
                    .map_err(|e| WorkRunMigrationError::InvalidInput(e.to_string()))?,
            );
            continue;
        }
        let worker_id = binding
            .worker_id
            .as_ref()
            .ok_or_else(|| WorkRunMigrationError::InvalidInput("binding has no workerId".into()))?;
        let worker_instance_id = binding.worker_instance_id.as_ref().ok_or_else(|| {
            WorkRunMigrationError::InvalidInput("binding has no workerInstanceId".into())
        })?;
        let worker_session_id = binding.worker_session_id.as_ref().ok_or_else(|| {
            WorkRunMigrationError::InvalidInput("binding has no workerSessionId".into())
        })?;
        let lease_id = binding
            .lease_id
            .as_ref()
            .ok_or_else(|| WorkRunMigrationError::InvalidInput("binding has no leaseId".into()))?;
        let fencing = binding.fencing_token.as_ref().ok_or_else(|| {
            WorkRunMigrationError::InvalidInput("binding has no fencingToken".into())
        })?;
        let id = deterministic_prefixed_id(
            "wrn_",
            &format!("{}:{}:{}", snap.id.0, stage.id.0, stage.attempt),
        );
        let state = match stage.status {
            crate::domain::StageRunStatus::Succeeded => "settled",
            crate::domain::StageRunStatus::Failed => "failed",
            // Historical leases are never resumed by migration.
            crate::domain::StageRunStatus::Cancelled
            | crate::domain::StageRunStatus::Running
            | crate::domain::StageRunStatus::Waiting => "cancelled",
        };
        run_values.push(json!({"schemaVersion": "winwincode/v1", "id": id, "workContractId": deterministic_prefixed_id("wct_", &format!("{}:{}", snap.id.0, snap.spec.id.0)), "contractRevision": snap.spec.revision, "workItemId": deterministic_prefixed_id("wit_", &format!("{}:{}", snap.id.0, task_id.0)), "workItemRevision": snap.revision, "revision": snap.revision, "state": state, "executionJobId": binding.execution_job_id, "attempt": stage.attempt, "workerId": worker_id, "workerInstanceId": worker_instance_id, "workerSessionId": worker_session_id, "leaseId": lease_id, "fencingToken": fencing, "productSessionId": binding.product_session_id, "codexThreadId": binding.codex_thread_id, "candidateDigest": Value::Null}));
    }
    let contract_id =
        deterministic_prefixed_id("wct_", &format!("{}:{}", snap.id.0, snap.spec.id.0));
    let work_items: Vec<Value> = snap
        .tasks
        .iter()
        .map(|task| {
            json!({
                "schemaVersion":"winwincode/v1", "id":deterministic_prefixed_id("wit_", &format!("{}:{}", snap.id.0, task.id.0)),
                "workContractId":contract_id, "workContractRevision":snap.spec.revision,
                "revision":snap.revision, "state":if task.blocked_by_task_ids.is_empty() { "ready" } else { "waiting_dependency" }, "title":task.title,
                "goal":task.goal,
                "criterionIds":task.acceptance_criterion_ids.iter().map(|c| deterministic_prefixed_id("crt_", &format!("{}:{}", snap.id.0, c.0))).collect::<Vec<_>>(),
                "dependsOn":task.blocked_by_task_ids.iter().map(|d| deterministic_prefixed_id("wit_", &format!("{}:{}", snap.id.0, d.0))).collect::<Vec<_>>()
            })
        })
        .collect();
    let created_at = instant_from_millis(snap.spec.created_at_millis)?;
    let work_contract = json!({"schemaVersion":"winwincode/v1", "id":contract_id,
        "revision":snap.spec.revision, "objective":snap.spec.goal, "scope":snap.spec.scope,
        "protectedScope":snap.spec.out_of_scope, "constraints":snap.spec.constraints,
        "criteria":snap.spec.acceptance_criteria.iter().map(|c| json!({"id":deterministic_prefixed_id("crt_", &format!("{}:{}", snap.id.0, c.id.0)),"description":c.description,"required":c.required,"verificationMethod":c.verification_method})).collect::<Vec<_>>(), "requiredHumanAuthority":if snap.attention_items.is_empty() { "none" } else { "attention" },
        "createdAt":created_at});
    let source = serde_json::to_value(snap)
        .map_err(|e| WorkRunMigrationError::Transaction(e.to_string()))?;
    let out = json!({"schemaVersion":"winwincode/v1", "sourceDeliveryId":snap.id,
        "sourceDeliveryRevision":snap.revision, "workContract":work_contract, "workItems":work_items,
        "workRuns":run_values, "historicalStageRuns":historical, "historicalSource":source,
        "migration":{"schemaVersion":WORKRUN_MIGRATION_SCHEMA_VERSION, "inputSha256":hex_digest(input), "oldLeasesInvalidated":true}});
    serde_json::from_value::<WorkContract>(work_contract.clone())
        .map_err(|e| WorkRunMigrationError::InvalidInput(format!("typed WorkContract: {e}")))?;
    for item in &work_items {
        serde_json::from_value::<WorkItem>(item.clone())
            .map_err(|e| WorkRunMigrationError::InvalidInput(format!("typed WorkItem: {e}")))?;
    }
    for run in &run_values {
        serde_json::from_value::<WorkRun>(run.clone())
            .map_err(|e| WorkRunMigrationError::InvalidInput(format!("typed WorkRun: {e}")))?;
    }
    let bytes =
        serde_json::to_vec(&out).map_err(|e| WorkRunMigrationError::Transaction(e.to_string()))?;
    Ok((source_key, bytes))
}
fn hex_digest(input: &[u8]) -> String {
    format!("{:x}", Sha256::digest(input))
}
fn instant_from_millis(millis: u64) -> Result<String, WorkRunMigrationError> {
    let format = time::format_description::parse(
        "[year]-[month]-[day]T[hour]:[minute]:[second].[subsecond digits:3]Z",
    )
    .map_err(|e| WorkRunMigrationError::InvalidInput(format!("invalid Instant format: {e}")))?;
    let value = time::OffsetDateTime::from_unix_timestamp_nanos(i128::from(millis) * 1_000_000)
        .map_err(|e| WorkRunMigrationError::InvalidInput(format!("createdAt out of range: {e}")))?;
    if !(0..=9999).contains(&value.year()) {
        return Err(WorkRunMigrationError::InvalidInput(
            "createdAt year is outside the canonical Instant range".into(),
        ));
    }
    value
        .format(&format)
        .map_err(|e| WorkRunMigrationError::InvalidInput(format!("createdAt format: {e}")))
}

fn valid_id(prefix: &str, value: &str) -> bool {
    value.strip_prefix(prefix).is_some_and(|tail| {
        tail.len() == 26
            && tail.bytes().all(|b| {
                b.is_ascii_digit()
                    || matches!(b, b'A'..=b'H'|b'J'..=b'K'|b'M'..=b'N'|b'P'..=b'T'|b'V'..=b'Z')
            })
    })
}

fn deterministic_prefixed_id(prefix: &str, source: &str) -> String {
    const ALPHABET: &[u8; 32] = b"0123456789ABCDEFGHJKMNPQRSTVWXYZ";
    let digest = Sha256::digest(source.as_bytes());
    let mut bits = u128::from_be_bytes(digest[..16].try_into().expect("SHA-256 has 32 bytes"));
    let mut encoded = [b'0'; 26];
    for byte in encoded.iter_mut().rev() {
        *byte = ALPHABET[usize::try_from(bits & 31).expect("five-bit index")];
        bits >>= 5;
    }
    format!("{prefix}{}", String::from_utf8_lossy(&encoded))
}
