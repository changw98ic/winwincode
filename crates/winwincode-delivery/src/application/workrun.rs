// SPDX-License-Identifier: Apache-2.0
//! Controller-owned `WorkItem` scheduling seam for the `WorkRun` cutover.
//!
//! Selection is deliberately independent of Delivery's former global stage. A
//! Controller may start several ready items; each resulting run is settled by
//! one durable terminal fact carrying its own attempt identity.
use super::stage::{TerminalOutcomeStatus, VerifiedTerminalOutcome};
use std::collections::HashSet;
use winwincode_domain::is_canonical_prefixed_id;
use winwincode_domain::{
    Revision, SchemaVersion, WorkContract, WorkItem, WorkItemId, WorkItemState, WorkRun, WorkRunId,
    WorkRunState,
};
use winwincode_storage::ExecutionDispatchAuthority;

fn valid_revision(value: i64) -> bool {
    (1..=9_007_199_254_740_991).contains(&value)
}

fn bounded_text(value: &str, minimum: usize, maximum: usize) -> bool {
    (minimum..=maximum).contains(&value.chars().take(maximum + 1).count())
}

fn valid_text_list(values: &[String], unique: bool) -> bool {
    values.len() <= 1000
        && values.iter().all(|value| bounded_text(value, 1, 65_536))
        && (!unique || values.iter().collect::<HashSet<_>>().len() == values.len())
}

fn valid_created_at(value: &str) -> bool {
    let bytes = value.as_bytes();
    if bytes.len() != 24
        || [
            (4, b'-'),
            (7, b'-'),
            (10, b'T'),
            (13, b':'),
            (16, b':'),
            (19, b'.'),
            (23, b'Z'),
        ]
        .iter()
        .any(|(index, separator)| bytes[*index] != *separator)
    {
        return false;
    }
    let number = |start, end| {
        let digits = value.get(start..end)?;
        if digits.bytes().all(|byte| byte.is_ascii_digit()) {
            digits.parse::<u16>().ok()
        } else {
            None
        }
    };
    let Some((year, month, day, hour, minute, second, millis)) = (|| {
        Some((
            number(0, 4)?,
            u8::try_from(number(5, 7)?).ok()?,
            u8::try_from(number(8, 10)?).ok()?,
            u8::try_from(number(11, 13)?).ok()?,
            u8::try_from(number(14, 16)?).ok()?,
            u8::try_from(number(17, 19)?).ok()?,
            number(20, 23)?,
        ))
    })() else {
        return false;
    };
    let Ok(month) = time::Month::try_from(month) else {
        return false;
    };
    time::Date::from_calendar_date(i32::from(year), month, day).is_ok()
        && time::Time::from_hms_milli(hour, minute, second, millis).is_ok()
}

fn acyclic_items(items: &[WorkItem]) -> bool {
    // ponytail: bounded to 1000 items; use an indegree index if that ceiling grows.
    let mut resolved = HashSet::new();
    loop {
        let before = resolved.len();
        for item in items {
            if item.depends_on.iter().all(|id| resolved.contains(id)) {
                resolved.insert(item.id.clone());
            }
        }
        if resolved.len() == items.len() {
            return true;
        }
        if resolved.len() == before {
            return false;
        }
    }
}

fn invalid_run_fields(run: &WorkRun) -> bool {
    !valid_revision(run.revision.0)
        || !is_canonical_prefixed_id(&run.id.0, "wrn_")
        || !is_canonical_prefixed_id(&run.execution_job_id.0, "job_")
        || !is_canonical_prefixed_id(&run.worker_id.0, "wrk_")
        || !is_canonical_prefixed_id(&run.worker_instance_id.0, "wki_")
        || !is_canonical_prefixed_id(&run.worker_session_id.0, "wsn_")
        || !is_canonical_prefixed_id(&run.lease_id.0, "lse_")
        || run
            .product_session_id
            .as_ref()
            .is_some_and(|id| !is_canonical_prefixed_id(&id.0, "psn_"))
        || run
            .codex_thread_id
            .as_ref()
            .is_some_and(|id| !is_canonical_prefixed_id(&id.0, "cdx_"))
        || run.fencing_token.starts_with('0')
        || run.fencing_token.len() > 20
        || run.fencing_token.is_empty()
        || !run.fencing_token.bytes().all(|byte| byte.is_ascii_digit())
        || run.candidate_digest.as_ref().is_some_and(|digest| {
            !digest.0.strip_prefix("sha256:").is_some_and(|hex| {
                hex.len() == 64
                    && hex
                        .bytes()
                        .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
            })
        })
}

#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct WorkRunAggregate {
    pub schema_version: SchemaVersion,
    pub contract: WorkContract,
    pub items: Vec<WorkItem>,
    pub runs: Vec<WorkRun>,
}

impl Eq for WorkRunAggregate {}

impl WorkRunAggregate {
    /// Checks stored relationships before scheduling or accepting an appended run.
    /// A generated Rust struct alone does not validate references or uniqueness.
    ///
    /// # Errors
    /// Rejects invalid references, duplicate identities, cycles or out-of-range attempts.
    pub fn validate(&self) -> Result<(), WorkRunSchedulingError> {
        let invalid = WorkRunSchedulingError::InvalidAggregate;
        if !valid_revision(self.contract.revision.0)
            || !is_canonical_prefixed_id(&self.contract.id.0, "wct_")
            || !bounded_text(&self.contract.objective, 1, 65_536)
            || !valid_created_at(&self.contract.created_at.0)
            || !valid_text_list(&self.contract.scope, true)
            || !valid_text_list(&self.contract.protected_scope, true)
            || !valid_text_list(&self.contract.constraints, false)
            || !matches!(
                self.contract.required_human_authority.as_str(),
                "none" | "approval" | "attention"
            )
            || self.contract.criteria.iter().any(|criterion| {
                !is_canonical_prefixed_id(&criterion.id.0, "crt_")
                    || !bounded_text(&criterion.description, 1, 65_536)
                    || criterion
                        .verification_method
                        .as_ref()
                        .is_some_and(|value| !bounded_text(value, 0, 65_536))
            })
            || self.contract.criteria.is_empty()
            || self.contract.criteria.len() > 1000
            || self.items.len() > 1000
            || self.runs.len() > 1000
        {
            return Err(invalid);
        }
        let criterion_ids: HashSet<_> = self.contract.criteria.iter().map(|c| &c.id).collect();
        if criterion_ids.len() != self.contract.criteria.len() {
            return Err(invalid);
        }
        let item_ids: HashSet<_> = self.items.iter().map(|item| &item.id).collect();
        if item_ids.len() != self.items.len() {
            return Err(invalid);
        }
        for item in &self.items {
            if item.work_contract_id != self.contract.id
                || item.work_contract_revision != self.contract.revision
                || !valid_revision(item.revision.0)
                || !is_canonical_prefixed_id(&item.id.0, "wit_")
                || !bounded_text(&item.title, 1, 500)
                || !bounded_text(&item.goal, 1, 65_536)
                || item.criterion_ids.len() > 1000
                || item.depends_on.len() > 1000
                || item.criterion_ids.is_empty()
                || item.criterion_ids.iter().collect::<HashSet<_>>().len()
                    != item.criterion_ids.len()
                || item
                    .criterion_ids
                    .iter()
                    .any(|id| !criterion_ids.contains(id))
                || item.depends_on.iter().collect::<HashSet<_>>().len() != item.depends_on.len()
                || item
                    .depends_on
                    .iter()
                    .any(|id| id == &item.id || !item_ids.contains(id))
            {
                return Err(invalid);
            }
        }
        if !acyclic_items(&self.items) {
            return Err(invalid);
        }
        let mut run_ids = HashSet::new();
        // Attempts belong to one execution job. A candidate producer and its
        // read-only consumer share a WorkItem, so WorkItem+attempt is not
        // an identity key for the WorkRun aggregate.
        let mut attempts = HashSet::new();
        let mut active_items = HashSet::new();
        let mut candidate_items = HashSet::new();
        for run in &self.runs {
            let item = self
                .items
                .iter()
                .find(|item| item.id == run.work_item_id)
                .ok_or(WorkRunSchedulingError::InvalidAggregate)?;
            if run.work_contract_id != self.contract.id
                || run.contract_revision != self.contract.revision
                || !valid_revision(run.work_item_revision.0)
                || run.work_item_revision.0 > item.revision.0
                || invalid_run_fields(run)
                || !(1..=1000).contains(&run.attempt)
                || !run_ids.insert(&run.id)
                || !attempts.insert((&run.execution_job_id, run.attempt))
            {
                return Err(invalid);
            }
            if run.state == WorkRunState::CandidateReady
                && !candidate_items.insert(&run.work_item_id)
            {
                return Err(invalid);
            }
            if matches!(run.state, WorkRunState::Leased | WorkRunState::Running)
                && !active_items.insert(&run.work_item_id)
            {
                return Err(invalid);
            }
        }
        Ok(())
    }

    /// Selects from persisted runs/items; callers cannot supply an active-ID list.
    ///
    /// # Errors
    /// Rejects invalid snapshots, exhausted attempts or a graph with no runnable item.
    pub fn start_next(&self) -> Result<WorkRunStart, WorkRunSchedulingError> {
        self.validate()?;
        let active = self
            .runs
            .iter()
            .filter(|run| {
                matches!(
                    run.state,
                    WorkRunState::Leased | WorkRunState::Running | WorkRunState::CandidateReady
                )
            })
            .map(|run| run.work_item_id.clone())
            .collect();
        let start = start_next_work_run(
            &self.items,
            &WorkRunAdvanceInput {
                expected_contract_revision: self.contract.revision.clone(),
                active_work_item_ids: active,
            },
        )?;
        self.start_item(&start.work_item.id)
    }

    /// Selects one exact ready `WorkItem`, retaining dependency and active-run checks.
    ///
    /// # Errors
    /// Rejects stale or invalid items, blocked dependencies, active runs and exhausted attempts.
    pub fn start_item(&self, id: &WorkItemId) -> Result<WorkRunStart, WorkRunSchedulingError> {
        self.validate()?;
        let item = self
            .items
            .iter()
            .find(|item| {
                &item.id == id
                    && work_item_is_runnable(item, &self.items)
                    && !self.runs.iter().any(|run| {
                        &run.work_item_id == id
                            && matches!(
                                run.state,
                                WorkRunState::Leased
                                    | WorkRunState::Running
                                    | WorkRunState::CandidateReady
                            )
                    })
            })
            .ok_or(WorkRunSchedulingError::NoRunnableWorkItem)?;
        // Selection creates a new execution job; scheduler retries retain that job.
        let attempt = 1;
        let mut work_item = item.clone();
        work_item.state = WorkItemState::InProgress;
        Ok(WorkRunStart { work_item, attempt })
    }

    /// Selects an existing candidate item for a new, read-only execution job.
    /// The candidate producer and the item's input revision are retained.
    ///
    /// # Errors
    /// Rejects missing candidates, stale contracts, or an already running consumer.
    pub fn start_verification(
        &self,
        id: &WorkItemId,
    ) -> Result<WorkRunStart, WorkRunSchedulingError> {
        self.validate()?;
        let item = self
            .items
            .iter()
            .find(|item| &item.id == id && item.state == WorkItemState::CandidateReady)
            .ok_or(WorkRunSchedulingError::NoRunnableWorkItem)?;
        if !self
            .runs
            .iter()
            .any(|run| &run.work_item_id == id && run.state == WorkRunState::CandidateReady)
            || self.runs.iter().any(|run| {
                &run.work_item_id == id
                    && matches!(run.state, WorkRunState::Leased | WorkRunState::Running)
            })
        {
            return Err(WorkRunSchedulingError::NoRunnableWorkItem);
        }
        Ok(WorkRunStart {
            work_item: item.clone(),
            attempt: 1,
        })
    }

    pub(crate) fn append_verification_run(
        &mut self,
        run: WorkRun,
    ) -> Result<(), WorkRunSchedulingError> {
        self.append_run_mode(run, true)
    }

    /// Adds one Controller-created `WorkRun` after checking its contract/item binding.
    ///
    /// # Errors
    /// Rejects stale bindings, duplicate active runs or nonsequential attempts.
    pub fn append_run(&mut self, run: WorkRun) -> Result<(), WorkRunSchedulingError> {
        self.append_run_mode(run, false)
    }

    fn append_run_mode(
        &mut self,
        run: WorkRun,
        read_only: bool,
    ) -> Result<(), WorkRunSchedulingError> {
        self.validate()?;
        if read_only {
            self.start_verification(&run.work_item_id)?;
        }
        if !matches!(run.state, WorkRunState::Leased | WorkRunState::Running) {
            return Err(WorkRunSchedulingError::StaleWorkItem);
        }
        if run.work_contract_id != self.contract.id
            || run.contract_revision != self.contract.revision
        {
            return Err(WorkRunSchedulingError::ContractRevisionMismatch);
        }
        let item = self
            .items
            .iter()
            .find(|item| item.id == run.work_item_id)
            .ok_or(WorkRunSchedulingError::NoRunnableWorkItem)?;
        if !read_only && !work_item_is_runnable(item, &self.items) {
            return Err(WorkRunSchedulingError::NoRunnableWorkItem);
        }
        if item.work_contract_id != run.work_contract_id
            || item.work_contract_revision != run.contract_revision
            || item.revision != run.work_item_revision
        {
            return Err(WorkRunSchedulingError::StaleWorkItem);
        }
        if self.runs.iter().any(|existing| existing.id == run.id) {
            return Err(WorkRunSchedulingError::StaleWorkItem);
        }
        if !read_only
            && self.runs.iter().any(|existing| {
                existing.work_item_id == run.work_item_id
                    && matches!(
                        existing.state,
                        WorkRunState::Leased | WorkRunState::Running | WorkRunState::CandidateReady
                    )
            })
        {
            return Err(WorkRunSchedulingError::StaleWorkItem);
        }
        let expected_attempt = self
            .runs
            .iter()
            .filter(|previous| previous.execution_job_id == run.execution_job_id)
            .map(|previous| previous.attempt)
            .max()
            .unwrap_or(0)
            .checked_add(1)
            .ok_or(WorkRunSchedulingError::InvalidAttempt)?;
        let expected_attempt = if read_only { 1 } else { expected_attempt };
        if (read_only
            && self
                .runs
                .iter()
                .any(|existing| existing.execution_job_id == run.execution_job_id))
            || run.attempt != expected_attempt
            || !(1..=1000).contains(&run.attempt)
        {
            return Err(WorkRunSchedulingError::InvalidAttempt);
        }
        let mut next = self.clone();
        if !read_only && matches!(run.state, WorkRunState::Leased | WorkRunState::Running) {
            let started = next
                .items
                .iter_mut()
                .find(|item| item.id == run.work_item_id)
                .ok_or(WorkRunSchedulingError::StaleWorkItem)?;
            started.state = WorkItemState::InProgress;
        }
        next.runs.push(run);
        next.validate()?;
        *self = next;
        Ok(())
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct WorkRunAdvanceInput {
    pub expected_contract_revision: Revision,
    pub active_work_item_ids: HashSet<WorkItemId>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct WorkRunStart {
    pub work_item: WorkItem,
    pub attempt: i64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum WorkRunTerminalOutcome {
    CandidateReady,
    Failed,
    Cancelled,
}

#[derive(Debug, Clone, PartialEq)]
struct WorkRunTerminalFact {
    work_run: WorkRun,
    attempt: i64,
    outcome: WorkRunTerminalOutcome,
}

impl WorkRunTerminalFact {
    fn from_dispatch_authority(
        run: &WorkRun,
        authority: &ExecutionDispatchAuthority,
        verified: &VerifiedTerminalOutcome,
    ) -> Result<Self, WorkRunSchedulingError> {
        let lease = authority.lease();
        if verified.work_run_id() != &run.id
            || run.execution_job_id != lease.job_id
            || run.lease_id != lease.lease_id
            || run.worker_id != lease.worker_id
            || run.worker_instance_id != lease.worker_instance_id
            || &run.worker_session_id != authority.worker_session_id()
            || u64::try_from(run.attempt).ok() != Some(lease.attempt)
            || run.fencing_token != lease.fencing_token.0
            || verified.execution_job_id() != &lease.job_id
            || verified.lease_id() != &lease.lease_id
            || verified.worker_id() != &lease.worker_id
            || verified.worker_instance_id() != &lease.worker_instance_id
            || verified.attempt() != lease.attempt
            || verified.fencing_token() != &lease.fencing_token
            || verified.worker_session_id() != authority.worker_session_id()
        {
            return Err(WorkRunSchedulingError::StaleWorkItem);
        }
        Self::from_verified(run, verified)
    }

    fn from_verified(
        run: &WorkRun,
        verified: &VerifiedTerminalOutcome,
    ) -> Result<Self, WorkRunSchedulingError> {
        if verified.work_run_id() != &run.id
            || verified.execution_job_id() != &run.execution_job_id
            || verified.lease_id() != &run.lease_id
            || verified.worker_id() != &run.worker_id
            || verified.worker_instance_id() != &run.worker_instance_id
            || verified.worker_session_id() != &run.worker_session_id
            || verified.codex_thread_id() != run.codex_thread_id.as_ref()
            || u64::try_from(run.attempt).ok() != Some(verified.attempt())
            || verified.fencing_token().0 != run.fencing_token
        {
            return Err(WorkRunSchedulingError::StaleWorkItem);
        }
        let outcome = match verified.status() {
            TerminalOutcomeStatus::Succeeded => WorkRunTerminalOutcome::CandidateReady,
            TerminalOutcomeStatus::Failed | TerminalOutcomeStatus::InfrastructureError => {
                WorkRunTerminalOutcome::Failed
            }
            TerminalOutcomeStatus::Cancelled => WorkRunTerminalOutcome::Cancelled,
        };
        Ok(Self {
            work_run: run.clone(),
            attempt: run.attempt,
            outcome,
        })
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WorkRunSchedulingError {
    InvalidAggregate,
    ContractRevisionMismatch,
    NoRunnableWorkItem,
    InvalidAttempt,
    StaleWorkItem,
}

fn work_item_is_runnable(item: &WorkItem, items: &[WorkItem]) -> bool {
    item.state == WorkItemState::Ready
        && item.depends_on.iter().all(|dependency| {
            items.iter().any(|candidate| {
                candidate.id == *dependency && candidate.state == WorkItemState::Done
            })
        })
}

/// Select one unblocked item without requiring other items to be terminal.
///
/// # Errors
/// Returns an error when no unblocked ready item matches the revision.
fn start_next_work_run(
    items: &[WorkItem],
    input: &WorkRunAdvanceInput,
) -> Result<WorkRunStart, WorkRunSchedulingError> {
    let item = items
        .iter()
        .find(|item| {
            item.work_contract_revision == input.expected_contract_revision
                && work_item_is_runnable(item, items)
                && !input.active_work_item_ids.contains(&item.id)
        })
        .ok_or(WorkRunSchedulingError::NoRunnableWorkItem)?;
    let mut started = item.clone();
    started.state = WorkItemState::InProgress;
    let attempt = items
        .iter()
        .filter(|candidate| candidate.id == item.id)
        .map(|_| 0_i64)
        .max()
        .unwrap_or(0)
        + 1;
    Ok(WorkRunStart {
        work_item: started,
        attempt,
    })
}

impl WorkRunAggregate {
    /// Settles only the persisted run whose complete lease binding was verified.
    ///
    /// # Errors
    /// Rejects stale or mismatched run, lease, terminal evidence or revision.
    pub fn settle_verified_run(
        &mut self,
        run_id: &WorkRunId,
        authority: &ExecutionDispatchAuthority,
        verified: &VerifiedTerminalOutcome,
    ) -> Result<(), WorkRunSchedulingError> {
        self.validate()?;
        let run = self
            .runs
            .iter()
            .find(|run| &run.id == run_id)
            .ok_or(WorkRunSchedulingError::StaleWorkItem)?;
        let fact = WorkRunTerminalFact::from_dispatch_authority(run, authority, verified)?;
        self.settle_fact(&fact)
    }

    pub(crate) fn settle_verified_outcome(
        &mut self,
        verified: &VerifiedTerminalOutcome,
    ) -> Result<(), WorkRunSchedulingError> {
        self.validate()?;
        let run = self
            .runs
            .iter()
            .find(|run| &run.id == verified.work_run_id())
            .ok_or(WorkRunSchedulingError::StaleWorkItem)?;
        let fact = WorkRunTerminalFact::from_verified(run, verified)?;
        self.settle_fact(&fact)
    }

    pub(crate) fn settle_verification_outcome(
        &mut self,
        verified: &VerifiedTerminalOutcome,
    ) -> Result<(), WorkRunSchedulingError> {
        self.validate()?;
        let run = self
            .runs
            .iter_mut()
            .find(|run| &run.id == verified.work_run_id())
            .ok_or(WorkRunSchedulingError::StaleWorkItem)?;
        WorkRunTerminalFact::from_verified(run, verified)?;
        if !matches!(run.state, WorkRunState::Leased | WorkRunState::Running)
            || !self.items.iter().any(|item| {
                item.id == run.work_item_id
                    && item.revision == run.work_item_revision
                    && item.state == WorkItemState::CandidateReady
            })
        {
            return Err(WorkRunSchedulingError::StaleWorkItem);
        }
        let revision = run
            .revision
            .0
            .checked_add(1)
            .filter(|v| valid_revision(*v))
            .ok_or(WorkRunSchedulingError::StaleWorkItem)?;
        run.state = match verified.status() {
            TerminalOutcomeStatus::Succeeded => WorkRunState::Settled,
            TerminalOutcomeStatus::Failed | TerminalOutcomeStatus::InfrastructureError => {
                WorkRunState::Failed
            }
            TerminalOutcomeStatus::Cancelled => WorkRunState::Cancelled,
        };
        run.revision = Revision(revision);
        Ok(())
    }

    fn settle_fact(&mut self, fact: &WorkRunTerminalFact) -> Result<(), WorkRunSchedulingError> {
        let run_index = self
            .runs
            .iter()
            .position(|run| run.id == fact.work_run.id)
            .ok_or(WorkRunSchedulingError::StaleWorkItem)?;
        let run = &self.runs[run_index];
        if run != &fact.work_run
            || !matches!(run.state, WorkRunState::Leased | WorkRunState::Running)
            || fact.attempt != run.attempt
        {
            return Err(WorkRunSchedulingError::StaleWorkItem);
        }
        let item_index = self
            .items
            .iter()
            .position(|item| item.id == run.work_item_id)
            .ok_or(WorkRunSchedulingError::StaleWorkItem)?;
        if self.items[item_index].state != WorkItemState::InProgress
            || self.items[item_index].revision != run.work_item_revision
        {
            return Err(WorkRunSchedulingError::StaleWorkItem);
        }
        let (run_state, item_state) = match fact.outcome {
            WorkRunTerminalOutcome::CandidateReady => {
                (WorkRunState::CandidateReady, WorkItemState::CandidateReady)
            }
            WorkRunTerminalOutcome::Failed => (WorkRunState::Failed, WorkItemState::Failed),
            WorkRunTerminalOutcome::Cancelled => {
                (WorkRunState::Cancelled, WorkItemState::Cancelled)
            }
        };
        let next_run_revision = run
            .revision
            .0
            .checked_add(1)
            .filter(|revision| *revision <= 9_007_199_254_740_991)
            .ok_or(WorkRunSchedulingError::StaleWorkItem)?;
        self.runs[run_index].state = run_state;
        self.runs[run_index].revision = Revision(next_run_revision);
        self.items[item_index].state = item_state;
        // WorkItem revision identifies the accepted input, not execution status.
        // Delivery and WorkRun revisions already sequence this state transition.
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use winwincode_domain::{CriterionId, SchemaVersion, WorkContractId};

    fn item(id: &str, state: WorkItemState, deps: Vec<WorkItemId>) -> WorkItem {
        WorkItem {
            criterion_ids: vec![CriterionId("crt_01J00000000000000000000000".into())],
            depends_on: deps,
            goal: "goal".into(),
            id: WorkItemId(id.into()),
            revision: Revision(1),
            schema_version: SchemaVersion::WinwincodeV1,
            state,
            title: id.into(),
            work_contract_id: WorkContractId("wct_01J00000000000000000000000".into()),
            work_contract_revision: Revision(1),
        }
    }

    #[test]
    fn independent_items_start_while_another_run_is_active() {
        let a = item(
            "wit_01J00000000000000000000000",
            WorkItemState::InProgress,
            vec![],
        );
        let b = item(
            "wit_01J00000000000000000000001",
            WorkItemState::Ready,
            vec![],
        );
        let input = WorkRunAdvanceInput {
            expected_contract_revision: Revision(1),
            active_work_item_ids: [a.id.clone()].into_iter().collect(),
        };
        let started = start_next_work_run(&[a, b.clone()], &input).unwrap();
        assert_eq!(started.work_item.id, b.id);
        assert_eq!(started.work_item.state, WorkItemState::InProgress);
    }

    #[test]
    fn dependencies_gate_until_done() {
        let dep = item(
            "wit_01J00000000000000000000000",
            WorkItemState::Ready,
            vec![],
        );
        let child = item(
            "wit_01J00000000000000000000001",
            WorkItemState::Ready,
            vec![dep.id.clone()],
        );
        let input = WorkRunAdvanceInput {
            expected_contract_revision: Revision(1),
            active_work_item_ids: HashSet::new(),
        };
        assert_eq!(
            start_next_work_run(&[dep.clone(), child.clone()], &input)
                .unwrap()
                .work_item
                .id,
            dep.id
        );
        let mut done_dep = dep.clone();
        done_dep.state = WorkItemState::Done;
        let started = start_next_work_run(&[done_dep, child.clone()], &input).unwrap();
        assert_eq!(started.work_item.id, child.id);
    }
}

#[cfg(test)]
mod aggregate_tests {
    use super::*;

    fn aggregate() -> WorkRunAggregate {
        serde_json::from_value(serde_json::json!({
            "schemaVersion":"winwincode/v1", "contract":{"schemaVersion":"winwincode/v1","id":"wct_01J00000000000000000000000","revision":1,"objective":"objective","scope":["scope"],"protectedScope":["protected"],"constraints":["constraint"],"criteria":[{"id":"crt_01J00000000000000000000000","description":"criterion","required":true,"verificationMethod":null}],"requiredHumanAuthority":"none","createdAt":"2026-01-01T00:00:00.000Z"},
            "items":[
              {"schemaVersion":"winwincode/v1","id":"wit_01J00000000000000000000000","workContractId":"wct_01J00000000000000000000000","workContractRevision":1,"revision":1,"state":"ready","title":"one","goal":"one goal","criterionIds":["crt_01J00000000000000000000000"],"dependsOn":[]},
              {"schemaVersion":"winwincode/v1","id":"wit_01J00000000000000000000001","workContractId":"wct_01J00000000000000000000000","workContractRevision":1,"revision":1,"state":"ready","title":"two","goal":"two goal","criterionIds":["crt_01J00000000000000000000000"],"dependsOn":[]}
            ], "runs":[]
        })).unwrap()
    }

    fn run(aggregate: &WorkRunAggregate, attempt: i64) -> WorkRun {
        serde_json::from_value(serde_json::json!({
            "schemaVersion":"winwincode/v1", "id":"wrn_01J00000000000000000000000",
            "workContractId":aggregate.contract.id, "contractRevision":1,
            "workItemId":aggregate.items[0].id, "workItemRevision":1, "revision":1,
            "state":"running", "executionJobId":"job_01J00000000000000000000000",
            "attempt":attempt, "workerId":"wrk_01J00000000000000000000000",
            "workerInstanceId":"wki_01J00000000000000000000000",
            "workerSessionId":"wsn_01J00000000000000000000000",
            "leaseId":"lse_01J00000000000000000000000", "fencingToken":"1",
            "productSessionId":null
        }))
        .unwrap()
    }

    #[test]
    fn verification_reuses_candidate_item_without_restarting_writer() {
        let mut value = aggregate();
        let writer = run(&value, 1);
        value.append_run(writer.clone()).unwrap();
        assert!(value.start_verification(&writer.work_item_id).is_err());
        value
            .settle_fact(&WorkRunTerminalFact {
                work_run: writer.clone(),
                attempt: 1,
                outcome: WorkRunTerminalOutcome::CandidateReady,
            })
            .unwrap();
        let source = value.clone();
        let selected = value.start_verification(&writer.work_item_id).unwrap();
        assert_eq!(
            selected.attempt, 1,
            "new verification job has its own attempt counter"
        );
        let mut consumer = run(&value, 1);
        consumer.id.0 = "wrn_01J00000000000000000000001".into();
        consumer.execution_job_id.0 = "job_01J00000000000000000000001".into();
        consumer.work_item_revision = selected.work_item.revision;
        assert!(
            value.append_run(consumer.clone()).is_err(),
            "writer cannot reuse a live candidate"
        );
        assert_eq!(value, source);
        let mut stale = consumer.clone();
        stale.work_item_revision = Revision(2);
        assert!(value.append_verification_run(stale).is_err());
        assert_eq!(value, source);
        value.append_verification_run(consumer).unwrap();
        assert_eq!(value.items, source.items);
        assert_eq!(value.runs[0], source.runs[0]);
        assert!(
            value.start_verification(&writer.work_item_id).is_err(),
            "duplicate consumer is rejected"
        );
        value.validate().unwrap();
    }

    #[test]
    fn terminal_fact_rejects_a_task_edited_after_dispatch_without_mutation() {
        let mut value = aggregate();
        let dispatched = run(&value, 1);
        value.append_run(dispatched.clone()).unwrap();
        let fact = WorkRunTerminalFact {
            work_run: dispatched,
            attempt: 1,
            outcome: WorkRunTerminalOutcome::CandidateReady,
        };
        let mut unchanged = value.clone();
        unchanged
            .settle_fact(&fact)
            .expect("unchanged task can settle");
        value.items[0].revision = Revision(2);
        value.items[0].goal = "Revised acceptance goal".into();
        value
            .validate()
            .expect("historical run remains a valid record");
        let before = value.clone();
        assert_eq!(
            value.settle_fact(&fact),
            Err(WorkRunSchedulingError::StaleWorkItem)
        );
        assert_eq!(value, before);
    }

    #[test]
    fn malformed_relationships_and_cycles_never_schedule() {
        let original = aggregate();
        for mutation in 0..5 {
            let mut value = original.clone();
            match mutation {
                0 => value.items[0].work_contract_id.0.push('X'),
                1 => {
                    let id = value.items[0].criterion_ids[0].clone();
                    value.items[0].criterion_ids.push(id);
                }
                2 => value.items.push(value.items[0].clone()),
                3 => {
                    value.items[0].depends_on = vec![value.items[1].id.clone()];
                    value.items[1].depends_on = vec![value.items[0].id.clone()];
                }
                _ => value.contract.revision = Revision(0),
            }
            assert!(value.start_next().is_err(), "accepted mutation {mutation}");
        }
    }

    #[test]
    fn append_checks_attempt_sequence_and_rejection_is_atomic() {
        let mut value = aggregate();
        let mut terminal = run(&value, 1);
        terminal.state = WorkRunState::CandidateReady;
        assert_eq!(
            value.append_run(terminal),
            Err(WorkRunSchedulingError::StaleWorkItem)
        );
        assert!(value.runs.is_empty());
        for attempt in [0, 2, 1001, i64::MAX] {
            let before = value.clone();
            assert_eq!(
                value.append_run(run(&value, attempt)),
                Err(WorkRunSchedulingError::InvalidAttempt)
            );
            assert_eq!(value, before);
        }
        let mut historical = run(&value, 1);
        historical.state = WorkRunState::Failed;
        value.runs.push(historical);
        assert_eq!(value.start_next().unwrap().attempt, 1);
        let mut duplicate = run(&value, 1);
        duplicate.id.0 = "wrn_01J00000000000000000000001".into();
        assert_eq!(
            value.append_run(duplicate.clone()),
            Err(WorkRunSchedulingError::InvalidAttempt)
        );
        // A new job starts at one even after the old job exhausted its retries.
        value.runs[0].attempt = 1000;
        assert_eq!(value.start_next().unwrap().attempt, 1);
        duplicate.execution_job_id.0 = "job_01J00000000000000000000001".into();
        value
            .append_run(duplicate)
            .expect("new writer job starts at one");
    }

    #[test]
    fn stored_duplicate_attempt_or_active_item_is_rejected() {
        let mut value = aggregate();
        let first = run(&value, 1);
        let mut second = first.clone();
        second.id.0 = "wrn_01J00000000000000000000001".into();
        value.runs = vec![first, second];
        assert!(value.validate().is_err());
        value.runs[1].attempt = 2;
        for run in &mut value.runs {
            run.state = WorkRunState::Running;
        }
        assert!(value.validate().is_err());
    }

    #[test]
    fn terminal_fact_is_bound_to_one_persisted_run_and_replay_is_rejected() {
        let mut value = aggregate();
        let mut active = run(&value, 1);
        active.state = WorkRunState::Running;
        value.append_run(active.clone()).unwrap();
        let fact = WorkRunTerminalFact {
            work_run: active,
            attempt: 1,
            outcome: WorkRunTerminalOutcome::Failed,
        };
        let before = value.clone();
        let mut foreign = fact.clone();
        foreign.work_run.lease_id.0 = "lse_01J00000000000000000000001".into();
        assert_eq!(
            value.settle_fact(&foreign),
            Err(WorkRunSchedulingError::StaleWorkItem)
        );
        assert_eq!(value, before);
        value.settle_fact(&fact).unwrap();
        assert_eq!(value.runs[0].state, WorkRunState::Failed);
        assert_eq!(value.items[0].state, WorkItemState::Failed);
        assert_eq!(value.runs[0].revision, Revision(2));
        assert_eq!(value.items[0].revision, Revision(1));
        assert_eq!(value.items[1], before.items[1]);
        let settled = value.clone();
        assert_eq!(
            value.settle_fact(&fact),
            Err(WorkRunSchedulingError::StaleWorkItem)
        );
        assert_eq!(value, settled);
    }

    #[test]
    fn canonical_scalar_boundaries_are_checked_before_scheduling() {
        for mutation in 0..8 {
            let mut value = aggregate();
            match mutation {
                0 => {
                    value.contract.id.0 = "bad".into();
                    for item in &mut value.items {
                        item.work_contract_id = value.contract.id.clone();
                    }
                }
                1 => value.contract.objective.clear(),
                2 => {
                    value.contract.revision = Revision(i64::MAX);
                    for item in &mut value.items {
                        item.work_contract_revision = value.contract.revision.clone();
                    }
                }
                3 => value.items[0].title = "x".repeat(501),
                4 => value.items[0].id.0.clear(),
                5 => value.contract.protected_scope = vec!["same".into(), "same".into()],
                6 => value.contract.required_human_authority = "unrestricted".into(),
                _ => value.contract.created_at.0 = "2026-02-30T00:00:00.000Z".into(),
            }
            assert!(
                value.validate().is_err(),
                "accepted scalar mutation {mutation}"
            );
        }
        let mut valid = aggregate();
        valid.items[0].title = "字".repeat(500);
        valid.contract.scope.clear();
        valid.contract.constraints = vec!["repeat permitted by schema".into(); 2];
        assert!(valid.validate().is_ok());
        for mutation in 0..4 {
            let mut value = aggregate();
            let mut invalid = run(&value, 1);
            match mutation {
                0 => invalid.worker_session_id.0 = "wsn_invalid".into(),
                1 => invalid.fencing_token = "01".into(),
                2 => invalid.revision = Revision(i64::MAX),
                _ => {
                    invalid.candidate_digest =
                        Some(winwincode_domain::CandidateDigest("sha256:bad".into()));
                }
            }
            let before = value.clone();
            assert!(value.append_run(invalid).is_err());
            assert_eq!(value, before);
        }
    }

    #[test]
    fn aggregate_dispatches_independent_items_and_round_trips() {
        let aggregate = aggregate();
        let first = aggregate.start_next().unwrap();
        assert_eq!(first.work_item.id.0, "wit_01J00000000000000000000000");
        let bytes = serde_json::to_vec(&aggregate).unwrap();
        let restored: WorkRunAggregate = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(restored.contract.id, aggregate.contract.id);
        assert_eq!(restored.items.len(), 2);
        assert!(restored.runs.is_empty());
    }
}
