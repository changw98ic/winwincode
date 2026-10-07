// SPDX-License-Identifier: Apache-2.0
use crate::tools::execution_facts::digest;
use codex_state::ToolCellFact;
use codex_state::ToolCellLifecycle;
use codex_state::ToolDiagnostic;
use codex_state::ToolDiagnosticCall;
use codex_state::ToolDiagnosticKind;
use codex_state::ToolExecutionFact;
use codex_state::ToolRuntimeEvent;
use codex_state::ToolRuntimeFact;
use codex_state::ToolWaitFact;
use codex_state::ToolWaitState;
use std::collections::BTreeMap;
use std::collections::BTreeSet;

/// A bounded view of Core facts. Thresholds request model judgement, never stop execution.
#[derive(Default)]
pub(super) struct Detector {
    requests: BTreeMap<i64, ToolExecutionFact>,
    cells: BTreeMap<i64, ToolCellFact>,
    waits: BTreeMap<i64, ToolWaitFact>,
    sharing_operations: BTreeMap<i64, String>,
    progress: Option<i64>,
    progress_request_cutoff: i64,
    pub(super) cursor: i64,
}
impl Detector {
    pub(super) fn consume(&mut self, events: Vec<ToolRuntimeEvent>) {
        for event in events {
            self.cursor = self.cursor.max(event.sequence);
            match event.fact {
                ToolRuntimeFact::Request(fact) => {
                    self.requests.insert(fact.request_sequence, fact);
                }
                ToolRuntimeFact::Cell(fact) => {
                    self.cells.insert(fact.sequence, fact);
                }
                ToolRuntimeFact::Wait(fact) => {
                    self.waits.insert(fact.waiter_request_sequence, fact);
                }
                ToolRuntimeFact::Sharing(fact) => {
                    self.sharing_operations
                        .insert(fact.request_sequence, fact.operation_digest);
                }
                ToolRuntimeFact::Progress(_) => {
                    self.progress = Some(event.sequence);
                    self.progress_request_cutoff =
                        self.requests.keys().next_back().copied().unwrap_or(0);
                }
                ToolRuntimeFact::Reconciliation(_)
                | ToolRuntimeFact::InputBinding(_)
                | ToolRuntimeFact::InputValidation(_)
                | ToolRuntimeFact::WaiterCancellation(_)
                | ToolRuntimeFact::Diagnostic(_)
                | ToolRuntimeFact::DiagnosticResponse(_) => {}
            }
        }
        while self.requests.len() > 128 {
            self.requests.pop_first();
        }
        self.sharing_operations
            .retain(|sequence, _| self.requests.contains_key(sequence));
        while self.cells.len() > 128 {
            self.cells.pop_first();
        }
        while self.waits.len() > 128 {
            self.waits.pop_first();
        }
    }
    fn operation(&self, fact: &ToolExecutionFact) -> String {
        self.sharing_operations
            .get(&fact.request_sequence)
            .cloned()
            .unwrap_or_else(|| operation(fact))
    }
    pub(super) fn diagnose(&self, thread_id: &str, owner_id: &str) -> Vec<ToolDiagnostic> {
        let recent: Vec<_> = self
            .requests
            .values()
            .rev()
            .filter(|fact| {
                fact.request_sequence > self.progress_request_cutoff
                    && !matches!(
                        fact.request.tool_name.as_str(),
                        "exec" | "wait" | "functions.exec" | "functions.wait"
                    )
            })
            .take(32)
            .collect::<Vec<_>>()
            .into_iter()
            .rev()
            .collect();
        let mut diagnostics = Vec::new();
        let mut operations: BTreeMap<String, Vec<&ToolExecutionFact>> = BTreeMap::new();
        for fact in &recent {
            operations
                .entry(self.operation(fact))
                .or_default()
                .push(fact);
        }
        for (operation, calls) in &operations {
            if calls.len() >= 6 {
                diagnostics.push(self.make(thread_id, ToolDiagnosticKind::RepeatedOperation, operation, calls,
                    "Is this repeated operation justified, or are we revisiting the same state? Identify the next action that can advance the task."));
            }
        }
        if recent.len() >= 6 {
            let tail = &recent[recent.len() - 6..];
            let a = self.operation(tail[0]);
            let b = self.operation(tail[1]);
            if a != b
                && tail.iter().enumerate().all(|(index, fact)| {
                    self.operation(fact) == if index % 2 == 0 { &a } else { &b }.as_str()
                })
            {
                diagnostics.push(self.make(thread_id, ToolDiagnosticKind::AlternatingCycle, &digest(serde_json::json!(BTreeSet::from([a,b]))), tail,
                    "These operations repeatedly alternate. What verified change distinguishes this cycle, and should the approach change?"));
            }
        }
        let parents: BTreeSet<_> = recent
            .iter()
            .filter_map(|fact| fact.request.parent_call_id.as_deref())
            .collect();
        if recent.len() >= 12 && parents.len() >= 3 && operations.len() <= 4 {
            diagnostics.push(self.make(thread_id, ToolDiagnosticKind::BranchExpansion, &digest(serde_json::json!(operations.keys().collect::<Vec<_>>())), &recent,
                "Several branches revisit a small set of operations. Are the branches independent, or is repeated work expanding? Choose how to advance the task."));
        }
        if let Some(cycle) = self.wait_cycle(owner_id) {
            diagnostics.push(self.make(thread_id, ToolDiagnosticKind::WaitCycle, &digest(serde_json::json!(cycle.iter().map(|fact| &fact.request.cell_id).collect::<BTreeSet<_>>())), &cycle,
                "Core recorded a cycle between live waiting cells. Which participant can resolve it, and what change can unblock the wait?"));
        }
        diagnostics
    }
    fn make(
        &self,
        thread: &str,
        kind: ToolDiagnosticKind,
        identity: &str,
        calls: &[&ToolExecutionFact],
        question: &str,
    ) -> ToolDiagnostic {
        ToolDiagnostic {
            schema_version: 1,
            diagnostic_id: digest(serde_json::json!([
                "core-tool-diagnosis-v1",
                thread,
                kind,
                identity,
                self.progress
            ])),
            thread_id: thread.into(),
            kind,
            evidence_version: calls
                .iter()
                .map(|fact| fact.request_sequence)
                .max()
                .unwrap_or(1),
            progress_source_sequence: self.progress,
            evidence: calls
                .iter()
                .rev()
                .take(4)
                .rev()
                .map(|fact| ToolDiagnosticCall {
                    request_sequence: fact.request_sequence,
                    logical_id: fact.request.logical_id.clone(),
                    tool_name: fact.request.tool_name.clone(),
                    operation_digest: operation(fact),
                    parent_call_id: fact.request.parent_call_id.clone(),
                    cell_id: fact.request.cell_id.clone(),
                })
                .collect(),
            question: question.into(),
        }
    }
    fn wait_cycle(&self, owner: &str) -> Option<Vec<&ToolExecutionFact>> {
        let mut edges = BTreeMap::<i64, Vec<(i64, i64)>>::new();
        for wait in self
            .waits
            .values()
            .filter(|wait| wait.state == ToolWaitState::Waiting && wait.owner_id == owner)
        {
            let Some(target) = self.cells.get(&wait.target_cell_sequence) else {
                continue;
            };
            if target.owner_id != owner || target.lifecycle != ToolCellLifecycle::Live {
                continue;
            }
            let Some(request) = self.requests.get(&wait.waiter_request_sequence) else {
                continue;
            };
            let Some(source) = self.cells.values().find(|cell| {
                Some(&cell.cell_id) == request.request.cell_id.as_ref()
                    && cell.owner_id == owner
                    && cell.scope_id == request.request.scope_id
                    && cell.lifecycle == ToolCellLifecycle::Live
            }) else {
                continue;
            };
            edges
                .entry(source.sequence)
                .or_default()
                .push((target.sequence, request.request_sequence));
        }
        for start in edges.keys() {
            let mut path = Vec::new();
            let mut visiting = BTreeSet::new();
            if find_cycle(*start, *start, &edges, &mut visiting, &mut path) {
                return Some(
                    path.into_iter()
                        .filter_map(|sequence| self.requests.get(&sequence))
                        .collect(),
                );
            }
        }
        None
    }
}
fn operation(fact: &ToolExecutionFact) -> String {
    fact.attempt.as_ref().map_or_else(
        || {
            format!(
                "unadmitted:{}:{}",
                fact.request.tool_name, fact.request.binding
            )
        },
        |attempt| attempt.operation_digest.clone(),
    )
}
fn find_cycle(
    start: i64,
    current: i64,
    edges: &BTreeMap<i64, Vec<(i64, i64)>>,
    visiting: &mut BTreeSet<i64>,
    path: &mut Vec<i64>,
) -> bool {
    if path.len() >= 128 || !visiting.insert(current) {
        return false;
    }
    for (target, request) in edges.get(&current).into_iter().flatten() {
        path.push(*request);
        if *target == start || find_cycle(start, *target, edges, visiting, path) {
            return true;
        }
        path.pop();
    }
    visiting.remove(&current);
    false
}

#[cfg(test)]
#[path = "tool_diagnostic_detector_tests.rs"]
mod tests;
