// SPDX-License-Identifier: Apache-2.0

//! Bounded advisory state used to resume an interrupted Worker task.

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

/// Current task shown in an advisory recovery snapshot.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct HandoffCurrentTask {
    /// Exact execution Job.
    pub execution_job_id: ExecutionJobId,
    /// Exact `WorkItem` when this is a `WorkRun` job.
    pub work_item_id: Option<WorkItemId>,
    /// Bounded task title.
    pub title: String,
    /// Bounded task goal.
    pub goal: String,
}

/// Acceptance-criterion progress rebuilt from durable `ChangeBatch` receipts.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct HandoffCriterionProgress {
    /// Criteria covered by accepted batches.
    pub completed: Vec<CriterionId>,
    /// Criteria named by the current unsettled batch.
    pub current: Vec<CriterionId>,
    /// Criteria not yet covered by an accepted or current batch.
    pub remaining: Vec<CriterionId>,
}

/// Bounded Git working-tree summary.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct HandoffDirtySummary {
    /// Total entries reported by Git.
    pub total_entries: usize,
    /// First bounded set of Git short-status entries.
    pub entries: Vec<String>,
    /// Whether entries were omitted from this advisory view.
    pub truncated: bool,
}

/// Complete non-authoritative Worker recovery hint.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct WorkerHandoffSnapshot {
    /// This value is always false; Controller facts remain authoritative.
    pub canonical_fact: bool,
    /// Current active task.
    pub current_task: HandoffCurrentTask,
    /// Receipt-derived criterion progress.
    pub progress: HandoffCriterionProgress,
    /// Exact current Git commit.
    pub git_head: String,
    /// Bounded current Git status.
    pub dirty: HandoffDirtySummary,
    /// Most recent validation check whose name identifies a test.
    pub last_test: Option<ValidationCheckSummary>,
}

/// Secret-free handoff snapshot failure.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct HandoffSnapshotError;

impl fmt::Display for HandoffSnapshotError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("Worker handoff snapshot is unavailable")
    }
}

impl std::error::Error for HandoffSnapshotError {}

/// Builds one bounded recovery hint from the live checkout and durable receipts.
///
/// # Errors
///
/// Rejects unavailable Git state or an oversized/non-UTF-8 status result.
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
