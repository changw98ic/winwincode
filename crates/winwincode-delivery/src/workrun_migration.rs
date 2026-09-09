// SPDX-License-Identifier: Apache-2.0
//! Strict one-time conversion from the canonical Delivery aggregate.
use crate::application::workrun::WorkRunAggregate;
use crate::domain::{
    AttentionItem, Delivery, DeliveryId, DeliverySnapshot, DeliverySpec, DeliveryStatus,
    DeliveryTask, DeliveryVerdict, EvidenceRef, SessionBinding, StageRun,
};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use winwincode_domain::{WorkContract, WorkItem, WorkRun};

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct FrozenSessionBinding {
    schema_version: u8,
    id: String,
    delivery_id: DeliveryId,
    delivery_task_id: Option<String>,
    stage_run_id: String,
    product_session_id: String,
    execution_job_id: String,
    worker_session_id: Option<String>,
    codex_thread_id: Option<String>,
    bound_at_millis: u64,
    worker_id: Option<String>,
    worker_instance_id: Option<String>,
    lease_id: Option<String>,
    attempt: u64,
    fencing_token: Option<String>,
    source_provenance: crate::domain::SessionBindingSourceProvenance,
}

/// The frozen pre-cutover input. Only this offline converter accepts it.
#[derive(Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct FrozenDeliverySnapshot {
    schema_version: u8,
    id: DeliveryId,
    revision: u64,
    status: DeliveryStatus,
    spec: DeliverySpec,
    tasks: Vec<DeliveryTask>,
    stage_runs: Vec<StageRun>,
    session_bindings: Vec<Value>,
    attention_items: Vec<Value>,
    evidence: Vec<Value>,
    #[serde(deserialize_with = "crate::domain::deserialize_required_option")]
    verdict: Option<DeliveryVerdict>,
    created_at_millis: u64,
    updated_at_millis: u64,
}

impl FrozenDeliverySnapshot {
    fn into_validation_snapshot(
        self,
        aggregate: WorkRunAggregate,
        session_bindings: Vec<SessionBinding>,
    ) -> DeliverySnapshot {
        DeliverySnapshot {
            schema_version: self.schema_version,
            id: self.id,
            revision: self.revision,
            // Historical completion is validated separately and retained in the source.
            // It does not authorize completion of the new execution aggregate.
            status: DeliveryStatus::Draft,
            spec: self.spec,
            tasks: self.tasks,
            stage_runs: self.stage_runs,
            session_bindings,
            // Old evidence stays in historicalSource and never authorizes a new WorkRun.
            attention_items: Vec::new(),
            evidence: Vec::new(),
            verdict: None,
            created_at_millis: self.created_at_millis,
            updated_at_millis: self.updated_at_millis,
            work_run_aggregate: aggregate,
        }
    }
}

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
    let frozen: FrozenDeliverySnapshot = serde_json::from_slice(input)
        .map_err(|e| WorkRunMigrationError::InvalidInput(e.to_string()))?;
    validate_historical_facts(&frozen)?;
    let source = serde_json::to_value(&frozen)
        .map_err(|e| WorkRunMigrationError::InvalidInput(e.to_string()))?;
    let (work_contract, work_items) = project_contract_items(&frozen)?;
    let historical_bindings = normalize_legacy_bindings(&frozen)?;
    let aggregate = WorkRunAggregate {
        schema_version: winwincode_domain::SchemaVersion::WinwincodeV1,
        contract: serde_json::from_value(work_contract.clone())
            .map_err(|e| WorkRunMigrationError::InvalidInput(format!("typed WorkContract: {e}")))?,
        items: serde_json::from_value(Value::Array(work_items.clone()))
            .map_err(|e| WorkRunMigrationError::InvalidInput(format!("typed WorkItems: {e}")))?,
        runs: Vec::new(),
    };
    // Reuse Delivery's relationship validator after normalizing only the frozen
    // legacy binding envelope. The runtime decoder still rejects this input shape.
    let delivery =
        Delivery::try_from_snapshot(frozen.into_validation_snapshot(aggregate, Vec::new()))
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
        let mut bindings = historical_bindings
            .iter()
            .filter(|binding| binding.stage_run_id == stage.id.0);
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
            .is_some_and(|v| valid_id("wrk_", v))
            && binding
                .worker_instance_id
                .as_ref()
                .is_some_and(|v| valid_id("wki_", v))
            && binding
                .worker_session_id
                .as_ref()
                .is_some_and(|v| valid_id("wsn_", v))
            && binding
                .lease_id
                .as_ref()
                .is_some_and(|v| valid_id("lse_", v))
            && valid_id("job_", &binding.execution_job_id)
            && valid_id("psn_", &binding.product_session_id)
            && binding
                .codex_thread_id
                .as_ref()
                .is_some_and(|v| valid_id("cdx_", v))
            && binding
                .fencing_token
                .as_ref()
                .is_some_and(|v| v.parse::<u64>().is_ok_and(|n| n > 0))
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
fn validate_historical_facts(frozen: &FrozenDeliverySnapshot) -> Result<(), WorkRunMigrationError> {
    if matches!(
        frozen.status,
        DeliveryStatus::ReadyToDeliver | DeliveryStatus::Delivered
    ) && frozen
        .verdict
        .as_ref()
        .is_none_or(|verdict| verdict.status != crate::domain::DeliveryVerdictStatus::Pass)
    {
        return Err(WorkRunMigrationError::InvalidInput(
            "historical completed Delivery has no passing verdict".into(),
        ));
    }
    if let Some(verdict) = &frozen.verdict {
        crate::domain::verdict::validate(verdict, "historical verdict")
            .map_err(|error| WorkRunMigrationError::InvalidInput(error.to_string()))?;
        if verdict.delivery_id != frozen.id
            || verdict.delivery_spec_id != frozen.spec.id
            || verdict.criteria.iter().any(|criterion| {
                !frozen
                    .spec
                    .acceptance_criteria
                    .iter()
                    .any(|declared| declared.id == criterion.criterion_id)
                    || criterion.evidence_refs.iter().any(|id| {
                        !frozen.evidence.iter().any(|evidence| {
                            evidence.get("id").and_then(Value::as_str) == Some(id.0.as_str())
                        })
                    })
            })
        {
            return Err(WorkRunMigrationError::InvalidInput(
                "historical verdict references foreign criteria or Evidence".into(),
            ));
        }
    }
    for (kind, records) in [
        ("evidence", &frozen.evidence),
        ("attentionItems", &frozen.attention_items),
    ] {
        let mut ids = std::collections::HashSet::new();
        for raw in records {
            let mut normalized = raw.clone();
            let fields = normalized.as_object_mut().ok_or_else(|| {
                WorkRunMigrationError::InvalidInput(format!("{kind} must contain objects"))
            })?;
            if fields.contains_key("workRunId") {
                return Err(WorkRunMigrationError::InvalidInput(format!(
                    "{kind} is not frozen StageRun input"
                )));
            }
            let stage = fields.remove("stageRunId").ok_or_else(|| {
                WorkRunMigrationError::InvalidInput(format!("{kind} is missing stageRunId"))
            })?;
            if let Some(stage_id) = stage.as_str()
                && !frozen.stage_runs.iter().any(|run| run.id.0 == stage_id)
            {
                return Err(WorkRunMigrationError::InvalidInput(format!(
                    "{kind} references a foreign stage"
                )));
            }
            fields.insert("workRunId".into(), stage);
            if raw.get("deliveryId").and_then(Value::as_str) != Some(frozen.id.0.as_str())
                || raw.get("deliverySpecId").and_then(Value::as_str)
                    != Some(frozen.spec.id.0.as_str())
                || !ids.insert(raw.get("id").and_then(Value::as_str))
            {
                return Err(WorkRunMigrationError::InvalidInput(format!(
                    "{kind} identity is foreign or duplicated"
                )));
            }
            if kind == "evidence" {
                let evidence: EvidenceRef = serde_json::from_value(normalized)
                    .map_err(|error| WorkRunMigrationError::InvalidInput(error.to_string()))?;
                crate::domain::evidence::validate(&evidence, kind)
                    .map_err(|error| WorkRunMigrationError::InvalidInput(error.to_string()))?;
                if evidence.delivery_spec_revision != frozen.spec.revision
                    || !frozen.session_bindings.iter().any(|binding| {
                        binding.get("id").and_then(Value::as_str)
                            == Some(evidence.session_binding_id.0.as_str())
                            && binding.get("stageRunId") == raw.get("stageRunId")
                    })
                {
                    return Err(WorkRunMigrationError::InvalidInput(
                        "historical Evidence binding or revision is foreign".into(),
                    ));
                }
            } else {
                let attention: AttentionItem = serde_json::from_value(normalized)
                    .map_err(|error| WorkRunMigrationError::InvalidInput(error.to_string()))?;
                crate::domain::attention::validate(&attention, kind)
                    .map_err(|error| WorkRunMigrationError::InvalidInput(error.to_string()))?;
            }
        }
    }
    Ok(())
}

fn project_contract_items(
    snap: &FrozenDeliverySnapshot,
) -> Result<(Value, Vec<Value>), WorkRunMigrationError> {
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
    Ok((work_contract, work_items))
}

fn normalize_legacy_bindings(
    frozen: &FrozenDeliverySnapshot,
) -> Result<Vec<FrozenSessionBinding>, WorkRunMigrationError> {
    let mut bindings = Vec::with_capacity(frozen.session_bindings.len());
    let mut binding_ids = std::collections::HashSet::new();
    let mut bound_stages = std::collections::HashSet::new();
    for raw in &frozen.session_bindings {
        let binding: FrozenSessionBinding = serde_json::from_value(raw.clone()).map_err(|e| {
            WorkRunMigrationError::InvalidInput(format!("invalid frozen SessionBinding: {e}"))
        })?;
        let validation = || -> Result<(), crate::domain::DeliveryValidationError> {
            crate::domain::schema_version(binding.schema_version, "binding.schemaVersion")?;
            for (path, value) in [
                ("binding.id", binding.id.as_str()),
                (
                    "binding.productSessionId",
                    binding.product_session_id.as_str(),
                ),
                ("binding.executionJobId", binding.execution_job_id.as_str()),
                (
                    "binding.sourceProvenance.reference",
                    binding.source_provenance.reference(),
                ),
            ] {
                crate::domain::portable_identifier(value, path)?;
            }
            crate::domain::safe_non_negative(binding.bound_at_millis, "binding.boundAtMillis")
        };
        validation().map_err(|e| WorkRunMigrationError::InvalidInput(e.to_string()))?;
        let stage = frozen
            .stage_runs
            .iter()
            .find(|stage| stage.id.0 == binding.stage_run_id)
            .ok_or_else(|| {
                WorkRunMigrationError::InvalidInput(
                    "session binding references a missing stage".into(),
                )
            })?;
        if !binding_ids.insert(binding.id.clone())
            || !bound_stages.insert(binding.stage_run_id.clone())
        {
            return Err(WorkRunMigrationError::InvalidInput(
                "session binding identity or stage relation is not unique".into(),
            ));
        }
        if binding.delivery_id != frozen.id
            || binding.delivery_task_id.as_deref()
                != stage.delivery_task_id.as_ref().map(|id| id.0.as_str())
            || binding.attempt != stage.attempt
            || binding.bound_at_millis < stage.started_at_millis
        {
            return Err(WorkRunMigrationError::InvalidInput(
                "session binding does not match Delivery/task/attempt/time".into(),
            ));
        }
        let authority = [
            &binding.worker_id,
            &binding.worker_instance_id,
            &binding.lease_id,
            &binding.fencing_token,
        ];
        let present = authority.iter().filter(|value| value.is_some()).count();
        if present != 0 && present != authority.len() {
            return Err(WorkRunMigrationError::InvalidInput(
                "session binding has partial lease authority".into(),
            ));
        }
        if binding.codex_thread_id.is_some() && binding.worker_session_id.is_none() {
            return Err(WorkRunMigrationError::InvalidInput(
                "codex thread has no worker session".into(),
            ));
        }
        bindings.push(binding);
    }
    // These frozen records are used only to export terminal historical runs.
    Ok(bindings)
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
