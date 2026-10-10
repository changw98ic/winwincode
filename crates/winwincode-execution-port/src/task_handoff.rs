// SPDX-License-Identifier: Apache-2.0

//! Private advisory task records shared by persistence and model compaction.
//! Product state and execution authority remain owned by the Controller.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

use crate::generated::ExecutionJob;

/// Host-only budget for the complete context at a compaction checkpoint.
pub const TASK_HANDOFF_CONTEXT_MAX_TOKENS: usize = 30_000;

/// The same prompt is used by manual and automatic embedded Core compaction.
pub const TASK_HANDOFF_COMPACT_PROMPT: &str = r"Compress the conversation into a concise task-state handoff for the next agent.
Output only YAML with these seven top-level fields, in this order:
Task: current task identity, goal and active conversational scope
Status: observed task status
Workspace: actual workspace path and known revision
Validation: actual verification evidence and its scope, or absence of evidence
Changes: file paths mapped to concise descriptions of changes already made
Dependencies: necessary references not already listed in Changes
Unverified: missing results and unresolved observations

Use the conversation's language. Keep all necessary current task state. Remove repetition and obsolete process history.
Use the host task record for sealed identity and receipt-derived facts. Preserve active conversational scope from the latest user input or handoff even when the sealed goal remains unchanged. Replace stale progress with newer actual tool receipts. Host observations can lag Core receipts; reconcile newer observed results. Source IDs locate evidence; they do not prove acceptance by themselves.
Distinguish new code from reuse of existing functions, applied changes from proposals, command success from independent acceptance, and received answers from actual waiter consumption. Preserve original deadlines when relevant. Missing results remain unverified. A call whose result is absent is not a failed call.
List each path once. Preserve exact task IDs, workspace/revision, important function names and result-source references. Leave root-cause guesses, investigation advice and process history out of the handoff.
Current host execution bindings, role instructions, constraints and authorization are supplied separately on each model request. Omit a rule only when that binding or current instructions actually restore it. Preserve active user scope changes, temporary boundaries and decisions that exist only in the conversation under Task. Never assume AGENTS.md restores conversational authorization.
The handoff is advisory. It does not advance product state or grant execution authority.
";

/// Stable task identity and the source revision of this execution.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct HandoffTask {
    /// Exact execution Job, rather than an inferred conversational label.
    pub job_id: String,
    /// Current `WorkItem`, when the execution belongs to a `WorkRun`.
    pub work_item_id: Option<String>,
    /// Sealed goal; later conversational scope remains in Core history.
    pub goal: String,
    /// Execution role, kept separate from task completion authority.
    pub role: String,
    /// Checkout source revision, not a claim about subsequent file changes.
    pub source_revision: String,
    /// Latest advisory internal checklist, copied from Core's plan event.
    pub plan: Option<serde_json::Value>,
}

/// An observed successful patch operation, not a semantic implementation claim.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct HandoffChange {
    /// Create, modify, delete or move, derived from Core's patch result.
    pub operation: String,
    /// Exact Core turn/tool call that produced this observation.
    pub source_id: String,
    /// Workspace-relative destination of an observed move, when applicable.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub destination: Option<String>,
}

/// One completed command check; it never implies independent acceptance.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct HandoffCheck {
    /// Exact Core turn/tool call that produced this result.
    pub source_id: String,
    /// Bounded command label; raw output stays in existing receipts.
    pub command: String,
    /// Actual command working directory, so checks retain their execution scope.
    pub cwd: String,
    /// Actual terminal command state.
    pub status: String,
    /// Actual observed process exit code.
    pub exit_code: i32,
}

/// Verification observations kept separate from product acceptance.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct HandoffValidation {
    /// This local record does not contain a Controller acceptance verdict.
    pub acceptance: String,
    /// Most recent bounded checks, in observation order.
    pub checks: Vec<HandoffCheck>,
}

/// Latest structured internal task record, persisted in the existing run store.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "PascalCase", deny_unknown_fields)]
pub struct TaskHandoffRecord {
    /// Exact task and execution identity.
    pub task: HandoffTask,
    /// Advisory execution status; completed checklists do not change it.
    pub status: String,
    /// Actual checkout path for relative file references.
    pub workspace: String,
    /// Recorded checks and explicitly separate acceptance state.
    pub validation: HandoffValidation,
    /// Latest observed successful patch per file.
    pub changes: BTreeMap<String, HandoffChange>,
    /// Sealed `WorkItem` dependencies; no speculative files are added.
    pub dependencies: Vec<String>,
    /// Outstanding operations and bounded-view omissions.
    pub unverified: BTreeMap<String, String>,
}

impl TaskHandoffRecord {
    /// Seeds an advisory record directly from the sealed execution input.
    #[must_use]
    pub fn from_job(job: &ExecutionJob, workspace: &str) -> Self {
        Self {
            task: HandoffTask {
                job_id: job.job_id.0.clone(),
                work_item_id: job
                    .work_input
                    .as_ref()
                    .map(|input| input.work_item.id.0.clone()),
                goal: job.goal.clone(),
                role: job.execution_profile.clone(),
                source_revision: job.workspace.checkout_revision.clone(),
                plan: None,
            },
            status: "in_progress".to_owned(),
            workspace: workspace.to_owned(),
            validation: HandoffValidation {
                acceptance: "unverified_by_controller".to_owned(),
                checks: Vec::new(),
            },
            changes: BTreeMap::new(),
            dependencies: job.work_input.as_ref().map_or_else(Vec::new, |input| {
                input
                    .work_item
                    .depends_on
                    .iter()
                    .map(|id| id.0.clone())
                    .collect()
            }),
            unverified: BTreeMap::new(),
        }
    }

    /// Renders valid YAML using JSON values, avoiding a second serialization dependency.
    ///
    /// # Errors
    /// Returns a serialization error rather than an incomplete handoff.
    pub fn to_yaml(&self) -> Result<String, serde_json::Error> {
        // Struct order is part of the human-facing template.
        let values = [
            ("Task", serde_json::to_value(&self.task)?),
            ("Status", serde_json::to_value(&self.status)?),
            ("Workspace", serde_json::to_value(&self.workspace)?),
            ("Validation", serde_json::to_value(&self.validation)?),
            ("Changes", serde_json::to_value(&self.changes)?),
            ("Dependencies", serde_json::to_value(&self.dependencies)?),
            ("Unverified", serde_json::to_value(&self.unverified)?),
        ];
        let mut text = String::new();
        for (field, value) in values {
            text.push_str(field);
            text.push_str(": ");
            text.push_str(&serde_json::to_string(&value)?);
            text.push('\n');
        }
        Ok(text)
    }
}
