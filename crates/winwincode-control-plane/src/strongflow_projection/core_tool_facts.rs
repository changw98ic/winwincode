// SPDX-License-Identifier: Apache-2.0
use super::sources::TrustedProjectionReadError;
use base64::{Engine as _, engine::general_purpose::STANDARD};
use serde_json::Value;
use sha2::{Digest, Sha256};
use winwincode_delivery::projection::runtime::{
    RuntimeActivityOutcome, RuntimeActivityProjection, RuntimeActivityStatus, RuntimeActivityType,
};
use winwincode_execution_port::generated::{CoreToolRuntimeFactPayload, ExecutionEventRecord};

/// Read-only projection of versioned Core receipts. Recorded running/live states
/// do not prove that an earlier Core owner or external handle is still alive.
#[derive(Default)]
pub(super) struct CoreToolProjector {
    relations: super::core_tool_relations::CoreToolRelations,
}
impl CoreToolProjector {
    pub(super) fn decode(
        &mut self,
        event: &ExecutionEventRecord,
        source_ref: String,
    ) -> Result<Option<RuntimeActivityProjection>, TrustedProjectionReadError> {
        let Some(payload) = &event.payload else {
            return Ok(None);
        };
        if payload.content_type != "application/vnd.winwincode.core-tool-fact+json" {
            return Ok(None);
        }
        let invalid = || TrustedProjectionReadError::Invalid;
        if payload.data_base64.len() > 96 * 1024 {
            return Err(invalid());
        }
        let bytes = STANDARD
            .decode(&payload.data_base64)
            .map_err(|_| invalid())?;
        if payload.payload_digest.0 != format!("sha256:{:x}", Sha256::digest(&bytes)) {
            return Err(invalid());
        }
        let source: CoreToolRuntimeFactPayload =
            serde_json::from_slice(&bytes).map_err(|_| invalid())?;
        if source.source_sequence.0 <= 0
            || source.source_thread_id.is_empty()
            || source.fact_json.len() > 32_768
        {
            return Err(invalid());
        }
        let mut projection = project(&source, source_ref)?;
        let value: Value = serde_json::from_str(&source.fact_json).map_err(|_| invalid())?;
        let meta = self.relations.project(&source, &value)?;
        if let Some(sequence) = value["fact"]["request_sequence"].as_i64() {
            projection.call_id = format!("core:{}:request:{sequence}", source.source_thread_id);
        }

        if let Some(call) = &meta.call {
            projection.call_id = format!(
                "core:{}:request:{}",
                source.source_thread_id, call.request_sequence.0
            );
            projection.command = Some(call.tool_name.clone());
            (projection.status, projection.outcome) = if call.cancelled {
                (
                    RuntimeActivityStatus::Cancelled,
                    RuntimeActivityOutcome::Cancelled,
                )
            } else if call.disposition
                == Some(winwincode_domain::CoreToolOutputDisposition::Rejected)
                || value["fact"]["resolution"] == "denied"
            {
                (
                    RuntimeActivityStatus::Declined,
                    RuntimeActivityOutcome::PolicyDenied,
                )
            } else if call.delivery == Some(winwincode_domain::CoreToolOutputDelivery::Offered) {
                (
                    RuntimeActivityStatus::Completed,
                    RuntimeActivityOutcome::Observed,
                )
            } else {
                (
                    RuntimeActivityStatus::Unknown,
                    RuntimeActivityOutcome::Observed,
                )
            };
        }
        if meta.diagnosis.is_some() && value["kind"] == "diagnostic" {
            projection.command = Some("Model runtime diagnosis".into());
        }
        projection.core_tool = Some(Box::new(meta));
        Ok(Some(projection))
    }
}

#[cfg(test)]
fn decode(
    event: &ExecutionEventRecord,
    source_ref: String,
) -> Result<Option<RuntimeActivityProjection>, TrustedProjectionReadError> {
    CoreToolProjector::default().decode(event, source_ref)
}

fn project(
    source: &CoreToolRuntimeFactPayload,
    source_ref: String,
) -> Result<RuntimeActivityProjection, TrustedProjectionReadError> {
    let invalid = || TrustedProjectionReadError::Invalid;
    let value: Value = serde_json::from_str(&source.fact_json).map_err(|_| invalid())?;
    let fact = &value["fact"];
    if fact["schema_version"] != 1 {
        return Err(invalid());
    }
    let (identity, summary, status, outcome) = match value["kind"].as_str() {
        Some("request") => request_projection(fact)?,
        Some("cell") => {
            let sequence = number(&fact["sequence"])?;
            let cell = text(&fact["cell_id"])?;
            let status = match fact["lifecycle"].as_str() {
                Some("closed") => RuntimeActivityStatus::Completed,
                Some("live") => RuntimeActivityStatus::Unknown,
                _ => return Err(invalid()),
            };
            (
                format!("cell:{sequence}"),
                format!("Code Mode cell {cell}"),
                status,
                RuntimeActivityOutcome::Observed,
            )
        }
        Some(kind @ ("diagnostic" | "diagnostic_response")) => diagnostic_projection(kind, fact)?,
        Some("progress") => {
            let sequence = number(&fact["request_sequence"])?;
            (
                format!("progress:{sequence}"),
                text(&fact["source"])?,
                RuntimeActivityStatus::Completed,
                RuntimeActivityOutcome::Observed,
            )
        }
        Some(kind @ ("waiter_cancellation" | "sharing" | "input_binding" | "input_validation")) => {
            input_projection(kind, fact)?
        }
        Some("reconciliation") => {
            let sequence = number(&fact["request_sequence"])?;
            let state = text(&fact["evidence"]["state"])?;
            if !matches!(
                state.as_str(),
                "unavailable" | "unconfirmed" | "running" | "exited"
            ) {
                return Err(invalid());
            }
            (
                format!("reconciliation:{sequence}"),
                format!("Tool recovery: {state}"),
                RuntimeActivityStatus::Unknown,
                RuntimeActivityOutcome::Observed,
            )
        }
        Some("wait") => {
            let waiter = number(&fact["waiter_request_sequence"])?;
            let target = number(&fact["target_cell_sequence"])?;
            let status = match fact["state"].as_str() {
                Some("settled") => RuntimeActivityStatus::Completed,
                Some("waiting") => RuntimeActivityStatus::Unknown,
                _ => return Err(invalid()),
            };
            (
                format!("wait:{waiter}"),
                format!("Code Mode wait for cell {target}"),
                status,
                RuntimeActivityOutcome::Observed,
            )
        }
        Some("agent_wait") => {
            let waiter = number(&fact["edge"]["request_sequence"])?;
            let status = match fact["state"].as_str() {
                Some("settled") => RuntimeActivityStatus::Completed,
                Some("waiting") => RuntimeActivityStatus::Unknown,
                _ => return Err(invalid()),
            };
            (
                format!("agent-wait:{waiter}"),
                "Agent completion wait".into(),
                status,
                RuntimeActivityOutcome::Observed,
            )
        }
        _ => return Err(invalid()),
    };
    Ok(RuntimeActivityProjection {
        core_tool: None,
        call_id: format!("core:{}:{identity}", source.source_thread_id),
        activity_type: RuntimeActivityType::Tool,
        command: Some(summary),
        status,
        outcome,
        exit_code: None,
        source_ref,
    })
}

fn text(field: &Value) -> Result<String, TrustedProjectionReadError> {
    field
        .as_str()
        .map(str::to_owned)
        .filter(|value| !value.is_empty())
        .ok_or(TrustedProjectionReadError::Invalid)
}
fn number(field: &Value) -> Result<i64, TrustedProjectionReadError> {
    field
        .as_i64()
        .filter(|value| *value > 0)
        .ok_or(TrustedProjectionReadError::Invalid)
}
fn request_projection(
    fact: &Value,
) -> Result<
    (
        String,
        String,
        RuntimeActivityStatus,
        RuntimeActivityOutcome,
    ),
    TrustedProjectionReadError,
> {
    let logical = text(&fact["request"]["logical_id"])?;
    let tool = text(&fact["request"]["tool_name"])?;
    let attempt = &fact["attempt"];
    let (status, outcome) =
        if fact["resolution"] == "denied" || attempt["disposition"] == "rejected" {
            (
                RuntimeActivityStatus::Declined,
                RuntimeActivityOutcome::PolicyDenied,
            )
        } else if attempt["execution"] == "completed" {
            (
                RuntimeActivityStatus::Completed,
                RuntimeActivityOutcome::Observed,
            )
        } else {
            (
                RuntimeActivityStatus::Unknown,
                RuntimeActivityOutcome::Observed,
            )
        };
    Ok((format!("request:{logical}"), tool, status, outcome))
}

#[cfg(test)]
#[path = "core_tool_facts_tests.rs"]
mod tests;

fn input_projection(
    kind: &str,
    fact: &Value,
) -> Result<
    (
        String,
        String,
        RuntimeActivityStatus,
        RuntimeActivityOutcome,
    ),
    TrustedProjectionReadError,
> {
    Ok(match kind {
        "waiter_cancellation" => {
            let sequence = number(&fact["request_sequence"])?;
            let source = number(&fact["source_request_sequence"])?;
            (
                format!("waiter-cancellation:{sequence}"),
                format!("Logical waiter cancelled for request {source}"),
                RuntimeActivityStatus::Completed,
                RuntimeActivityOutcome::Observed,
            )
        }
        "sharing" => {
            let sequence = number(&fact["request_sequence"])?;
            let source = number(&fact["source_request_sequence"])?;
            let kind = text(&fact["kind"])?;
            (
                format!("sharing:{sequence}"),
                format!("Tool {kind} from request {source}"),
                if fact["delivery"] == "offered" {
                    RuntimeActivityStatus::Completed
                } else {
                    RuntimeActivityStatus::Unknown
                },
                RuntimeActivityOutcome::Observed,
            )
        }
        "input_binding" | "input_validation" => {
            let sequence = number(&fact["request_sequence"])?;
            (
                format!("{kind}:{sequence}"),
                if kind == "input_binding" {
                    "Trusted tool input bound".into()
                } else {
                    format!("Tool input validation: {}", text(&fact["validation"])?)
                },
                RuntimeActivityStatus::Unknown,
                RuntimeActivityOutcome::Observed,
            )
        }
        _ => return Err(TrustedProjectionReadError::Invalid),
    })
}

fn diagnostic_projection(
    kind: &str,
    fact: &Value,
) -> Result<
    (
        String,
        String,
        RuntimeActivityStatus,
        RuntimeActivityOutcome,
    ),
    TrustedProjectionReadError,
> {
    let invalid = || TrustedProjectionReadError::Invalid;
    Ok(match kind {
        "diagnostic_response" => {
            let id = text(&fact["diagnostic_id"])?;
            number(&fact["evidence_version"])?;
            (
                format!("diagnostic-response:{id}"),
                "Model response to runtime diagnosis".into(),
                RuntimeActivityStatus::Unknown,
                RuntimeActivityOutcome::Observed,
            )
        }
        "diagnostic" => {
            let diagnosis = &fact["diagnostic"];
            if diagnosis["schema_version"] != 1
                || !matches!(fact["delivery"].as_str(), Some("queued" | "offered"))
            {
                return Err(invalid());
            }
            let id = text(&diagnosis["diagnostic_id"])?;
            number(&diagnosis["evidence_version"])?;
            (
                format!("diagnostic:{id}"),
                text(&diagnosis["question"])?,
                RuntimeActivityStatus::Unknown,
                RuntimeActivityOutcome::Observed,
            )
        }
        _ => return Err(invalid()),
    })
}
