use std::collections::HashSet;
use std::fmt;
use std::path::Path;

use serde::{Deserialize, Serialize};
use winwincode_domain::{CriterionId, ExecutionJobId, WorkItemId};
use winwincode_execution_port::generated::{ChangeBatchProgressState, ValidationCheckSummary};

use crate::ActiveJob;
use crate::workspace::{git_output, rev_parse};
use crate::workspace_runtime::DelegatedBatchHistory;

const MAX_GIT_STATUS_BYTES: usize = 256 * 1024;
const MAX_DIRTY_ENTRIES: usize = 100;

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct HandoffCurrentTask {
    pub execution_job_id: ExecutionJobId,
    pub work_item_id: Option<WorkItemId>,
    pub title: String,
    pub goal: String,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct HandoffCriterionProgress {
    pub completed: Vec<CriterionId>,
    pub current: Vec<CriterionId>,
    pub remaining: Vec<CriterionId>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct HandoffDirtySummary {
    pub total_entries: usize,
    pub entries: Vec<String>,
    pub truncated: bool,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct WorkerHandoffSnapshot {
    pub canonical_fact: bool,
    pub current_task: HandoffCurrentTask,
    pub progress: HandoffCriterionProgress,
    pub git_head: String,
    pub dirty: HandoffDirtySummary,
    pub last_test: Option<ValidationCheckSummary>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct HandoffSnapshotError;

impl fmt::Display for HandoffSnapshotError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("Worker handoff snapshot is unavailable")
    }
}

impl std::error::Error for HandoffSnapshotError {}

pub fn build_worker_handoff_snapshot(
    active: &ActiveJob,
    checkout: &Path,
    history: &[DelegatedBatchHistory],
) -> Result<WorkerHandoffSnapshot, HandoffSnapshotError> {
    let (current_task, criterion_ids) = active.job.work_input.as_ref().map_or_else(
        || {
            (
                HandoffCurrentTask {
                    execution_job_id: active.job.job_id.clone(),
                    work_item_id: None,
                    title: active.job.goal.clone(),
                    goal: active.job.goal.clone(),
                },
                Vec::new(),
            )
        },
        |input| {
            (
                HandoffCurrentTask {
                    execution_job_id: active.job.job_id.clone(),
                    work_item_id: Some(input.work_item.id.clone()),
                    title: input.work_item.title.clone(),
                    goal: input.work_item.goal.clone(),
                },
                input.work_item.criterion_ids.clone(),
            )
        },
    );
    let completed = history
        .iter()
        .filter(|entry| entry.terminal_state == ChangeBatchProgressState::Accepted)
        .flat_map(|entry| &entry.proposal.proposal.acceptance_criteria_ids)
        .cloned()
        .collect::<HashSet<_>>();
    let current = history
        .last()
        .filter(|entry| entry.terminal_state != ChangeBatchProgressState::Accepted)
        .map(|entry| {
            entry
                .proposal
                .proposal
                .acceptance_criteria_ids
                .iter()
                .cloned()
                .collect::<HashSet<_>>()
        })
        .unwrap_or_default();
    let progress = HandoffCriterionProgress {
        completed: criterion_ids
            .iter()
            .filter(|id| completed.contains(&id.0))
            .cloned()
            .collect(),
        current: criterion_ids
            .iter()
            .filter(|id| !completed.contains(&id.0) && current.contains(&id.0))
            .cloned()
            .collect(),
        remaining: criterion_ids
            .iter()
            .filter(|id| !completed.contains(&id.0) && !current.contains(&id.0))
            .cloned()
            .collect(),
    };
    let status = git_output(checkout, &["status", "--short", "--untracked-files=normal"])
        .map_err(|_| HandoffSnapshotError)?;
    if status.len() > MAX_GIT_STATUS_BYTES {
        return Err(HandoffSnapshotError);
    }
    let status = std::str::from_utf8(&status).map_err(|_| HandoffSnapshotError)?;
    let entries = status.lines().collect::<Vec<_>>();
    let dirty = HandoffDirtySummary {
        total_entries: entries.len(),
        entries: entries
            .iter()
            .take(MAX_DIRTY_ENTRIES)
            .map(|entry| entry.chars().take(500).collect())
            .collect(),
        truncated: entries.len() > MAX_DIRTY_ENTRIES,
    };
    let last_test = history.iter().rev().find_map(|entry| {
        entry
            .receipt
            .validation
            .as_ref()?
            .checks
            .iter()
            .rev()
            .find(|check| check.name.to_ascii_lowercase().contains("test"))
            .cloned()
    });
    Ok(WorkerHandoffSnapshot {
        canonical_fact: false,
        current_task,
        progress,
        git_head: rev_parse(checkout, "HEAD^{commit}").map_err(|_| HandoffSnapshotError)?,
        dirty,
        last_test,
    })
}
