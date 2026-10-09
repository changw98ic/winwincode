// SPDX-License-Identifier: Apache-2.0
//! Bounded read-only joins over ordered Core metadata receipts.
use super::sources::TrustedProjectionReadError;
use serde_json::{Value, json};
use std::collections::{BTreeMap, VecDeque};
use winwincode_domain::CoreToolRuntimeProjection;
use winwincode_execution_port::generated::CoreToolRuntimeFactPayload;

#[derive(Default)]
pub(super) struct CoreToolRelations {
    calls: BTreeMap<(String, i64), CoreToolRuntimeProjection>,
    order: VecDeque<(String, i64)>,
    cells: BTreeMap<(String, String), i64>,
    cell_order: VecDeque<(String, String)>,
}
impl CoreToolRelations {
    pub(super) fn project(
        &mut self,
        source: &CoreToolRuntimeFactPayload,
        value: &Value,
    ) -> Result<CoreToolRuntimeProjection, TrustedProjectionReadError> {
        let kind = value["kind"]
            .as_str()
            .ok_or(TrustedProjectionReadError::Invalid)?;
        let fact = &value["fact"];
        let sequence = fact["request_sequence"].as_i64();
        let key = sequence.map(|sequence| (source.source_thread_id.clone(), sequence));
        let previous = key.as_ref().and_then(|key| self.calls.get(key));
        let mut meta = json!({"sourceThreadId":source.source_thread_id,"sourceSequence":source.source_sequence.0,"kind":kind,"call":null,"cell":null,"wait":null,"diagnosis":null,"sharing":null,"recovery":null});
        if let Some(previous) = previous {
            meta["call"] = serde_json::to_value(&previous.call)
                .map_err(|_| TrustedProjectionReadError::Invalid)?;
            meta["sharing"] = serde_json::to_value(&previous.sharing)
                .map_err(|_| TrustedProjectionReadError::Invalid)?;
            meta["recovery"] = serde_json::to_value(&previous.recovery)
                .map_err(|_| TrustedProjectionReadError::Invalid)?;
        }
        match kind {
            "request" => {
                let request = &fact["request"];
                let attempt = &fact["attempt"];
                meta["call"] = json!({"requestSequence":fact["request_sequence"],"logicalId":request["logical_id"],"toolName":request["tool_name"],"parentCallId":request["parent_call_id"],"cellId":request["cell_id"],"attemptId":attempt["attempt_id"],"execution":attempt["execution"],"disposition":attempt["disposition"],"delivery":attempt["delivery"],"cancelled":previous.and_then(|p| p.call.as_ref()).is_some_and(|call| call.cancelled),"inputValidation":previous.and_then(|p| p.call.as_ref()).and_then(|call| call.input_validation.as_ref()),"parentRequestSequence":request["cell_id"].as_str().and_then(|cell| self.cells.get(&(source.source_thread_id.clone(),cell.to_owned())))});
            }
            "cell" => {
                meta["cell"] = json!({"sequence":fact["sequence"],"cellId":fact["cell_id"],"parentRequestSequence":fact["parent_request_sequence"],"lifecycle":fact["lifecycle"]});
            }
            "wait" => {
                meta["wait"] = json!({"requestSequence":fact["waiter_request_sequence"],"targetCellSequence":fact["target_cell_sequence"],"state":fact["state"]});
            }
            "agent_wait" => {
                meta["agentWait"] =
                    json!({"edge": wait_edge(&fact["edge"]), "state":fact["state"]});
            }
            "sharing" => {
                meta["sharing"] = json!({"sourceRequestSequence":fact["source_request_sequence"],"sourceAttemptId":fact["source_attempt_id"],"kind":fact["kind"]});
                if meta["call"].is_object() {
                    meta["call"]["disposition"] = fact["disposition"].clone();
                    meta["call"]["delivery"] = fact["delivery"].clone();
                    meta["call"]["cancelled"] = fact["cancelled"].clone();
                }
            }
            "waiter_cancellation" => {
                if meta["call"].is_object() {
                    meta["call"]["cancelled"] = json!(true);
                }
            }
            "input_validation" => {
                if meta["call"].is_object() {
                    meta["call"]["inputValidation"] = fact["validation"].clone();
                }
            }
            "reconciliation" => {
                meta["recovery"] = json!({"requestSequence":fact["request_sequence"],"state":fact["evidence"]["state"],"businessId":fact["evidence"]["business_id"],"exitCode":fact["evidence"]["exit_code"]});
            }
            "diagnostic" => {
                meta["diagnosis"] = diagnosis(&fact["diagnostic"], &fact["delivery"]);
            }
            "diagnostic_response" => {
                meta["diagnosis"] = json!({"diagnosticId":fact["diagnostic_id"],"evidenceVersion":fact["evidence_version"],"kind":null,"question":null,"evidence":[],"delivery":null});
            }
            "input_binding" | "progress" => {}
            _ => return Err(TrustedProjectionReadError::Invalid),
        }
        if kind == "cell" {
            let cell = fact["cell_id"]
                .as_str()
                .ok_or(TrustedProjectionReadError::Invalid)?;
            let parent = fact["parent_request_sequence"]
                .as_i64()
                .ok_or(TrustedProjectionReadError::Invalid)?;
            let key = (source.source_thread_id.clone(), cell.to_owned());
            if !self.cells.contains_key(&key) {
                self.cell_order.push_back(key.clone());
            }
            self.cells.insert(key, parent);
            while self.cell_order.len() > 256 {
                if let Some(key) = self.cell_order.pop_front() {
                    self.cells.remove(&key);
                }
            }
        }
        let projection: CoreToolRuntimeProjection =
            serde_json::from_value(meta).map_err(|_| TrustedProjectionReadError::Invalid)?;
        if let Some(key) = key {
            self.order.retain(|existing| existing != &key);
            self.order.push_back(key.clone());
            self.calls.insert(key, projection.clone());
            while self.order.len() > 256 {
                if let Some(key) = self.order.pop_front() {
                    self.calls.remove(&key);
                }
            }
        }
        Ok(projection)
    }
}
fn diagnosis(fact: &Value, delivery: &Value) -> Value {
    let evidence = fact["evidence"].as_array().into_iter().flatten().map(|call| json!({"requestSequence":call["request_sequence"],"logicalId":call["logical_id"],"toolName":call["tool_name"],"parentCallId":call["parent_call_id"],"cellId":call["cell_id"]})).collect::<Vec<_>>();
    let waits = fact["wait_graph"]
        .as_array()
        .into_iter()
        .flatten()
        .take(8)
        .map(wait_edge)
        .collect::<Vec<_>>();
    json!({"diagnosticId":fact["diagnostic_id"],"evidenceVersion":fact["evidence_version"],"kind":fact["kind"],"question":fact["question"],"evidence":evidence,"delivery":delivery,"waitGraph":waits})
}

fn wait_edge(edge: &Value) -> Value {
    let targets = edge["targets"].as_array();
    json!({"treeId":edge["tree_id"],"requestSequence":edge["request_sequence"],"logicalId":edge["logical_id"],
        "source":wait_node(&edge["source"]),"targets":targets.into_iter().flatten().take(8).map(wait_node).collect::<Vec<_>>(),
        "targetCount":targets.map_or(0, Vec::len),"deadlineUnixMs":edge["deadline_unix_ms"]})
}

fn wait_node(node: &Value) -> Value {
    json!({"kind":node["kind"],"threadId":node["thread_id"],"ownerId":node["owner_id"],"cellId":node["cell_id"],"scopeId":node["scope_id"]})
}
