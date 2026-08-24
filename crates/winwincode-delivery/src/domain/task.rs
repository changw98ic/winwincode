// SPDX-License-Identifier: Apache-2.0

use std::collections::{HashMap, HashSet};

use serde::{Deserialize, Serialize};
use winwincode_domain::{DeliveryId, DeliveryTaskId};

use super::{
    AcceptanceCriterionId, DeliveryValidationError, DeliveryValidationErrorCode, MAX_TEXT_LENGTH,
    bounded_text, collection_length, duplicate_ids, nullable_text, portable_identifier,
    schema_version, validation_error,
};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum DeliveryTaskStatus {
    #[serde(rename = "pending")]
    Pending,
    #[serde(rename = "active")]
    Active,
    #[serde(rename = "blocked")]
    Blocked,
    #[serde(rename = "verifying")]
    Verifying,
    #[serde(rename = "completed")]
    Completed,
    #[serde(rename = "failed")]
    Failed,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct DeliveryTask {
    pub schema_version: u8,
    pub id: DeliveryTaskId,
    pub delivery_id: DeliveryId,
    pub title: String,
    pub goal: String,
    pub acceptance_criterion_ids: Vec<AcceptanceCriterionId>,
    pub blocked_by_task_ids: Vec<DeliveryTaskId>,
    #[serde(deserialize_with = "super::deserialize_required_option")]
    pub owner: Option<String>,
    pub status: DeliveryTaskStatus,
}

pub(crate) fn validate(task: &DeliveryTask, path: &str) -> Result<(), DeliveryValidationError> {
    schema_version(task.schema_version, &format!("{path}.schemaVersion"))?;
    portable_identifier(&task.id.0, &format!("{path}.id"))?;
    portable_identifier(&task.delivery_id.0, &format!("{path}.deliveryId"))?;
    bounded_text(&task.title, &format!("{path}.title"), 256)?;
    bounded_text(&task.goal, &format!("{path}.goal"), MAX_TEXT_LENGTH)?;
    collection_length(
        task.acceptance_criterion_ids.len(),
        &format!("{path}.acceptanceCriterionIds"),
    )?;
    if task.acceptance_criterion_ids.is_empty() {
        return Err(validation_error(
            DeliveryValidationErrorCode::InvalidValue,
            format!("{path}.acceptanceCriterionIds"),
            "must not be empty",
        ));
    }
    for (index, criterion_id) in task.acceptance_criterion_ids.iter().enumerate() {
        portable_identifier(
            &criterion_id.0,
            &format!("{path}.acceptanceCriterionIds[{index}]"),
        )?;
    }
    duplicate_ids(
        task.acceptance_criterion_ids
            .iter()
            .map(|criterion_id| criterion_id.0.as_str()),
        &format!("{path}.acceptanceCriterionIds"),
    )?;
    collection_length(
        task.blocked_by_task_ids.len(),
        &format!("{path}.blockedByTaskIds"),
    )?;
    for (index, task_id) in task.blocked_by_task_ids.iter().enumerate() {
        portable_identifier(&task_id.0, &format!("{path}.blockedByTaskIds[{index}]"))?;
    }
    duplicate_ids(
        task.blocked_by_task_ids
            .iter()
            .map(|task_id| task_id.0.as_str()),
        &format!("{path}.blockedByTaskIds"),
    )?;
    nullable_text(task.owner.as_deref(), &format!("{path}.owner"), 500)
}

pub(crate) fn validate_graph(
    tasks: &[DeliveryTask],
    path: &str,
) -> Result<(), DeliveryValidationError> {
    let tasks_by_id: HashMap<&str, &DeliveryTask> = tasks
        .iter()
        .map(|task| (task.id.0.as_str(), task))
        .collect();
    for (index, task) in tasks.iter().enumerate() {
        for dependency in &task.blocked_by_task_ids {
            if dependency == &task.id || !tasks_by_id.contains_key(dependency.0.as_str()) {
                return Err(validation_error(
                    DeliveryValidationErrorCode::RelationshipMismatch,
                    format!("{path}[{index}].blockedByTaskIds"),
                    "delivery task dependency is missing or self-referential",
                ));
            }
        }
    }

    let mut visiting = HashSet::new();
    let mut visited = HashSet::new();
    for task in tasks {
        visit_task(task, &tasks_by_id, &mut visiting, &mut visited, path)?;
    }
    Ok(())
}

fn visit_task<'a>(
    task: &'a DeliveryTask,
    tasks: &HashMap<&'a str, &'a DeliveryTask>,
    visiting: &mut HashSet<&'a str>,
    visited: &mut HashSet<&'a str>,
    path: &str,
) -> Result<(), DeliveryValidationError> {
    let id = task.id.0.as_str();
    if visiting.contains(id) {
        return Err(validation_error(
            DeliveryValidationErrorCode::RelationshipMismatch,
            path,
            "delivery task dependencies contain a cycle",
        ));
    }
    if visited.contains(id) {
        return Ok(());
    }
    visiting.insert(id);
    for dependency in &task.blocked_by_task_ids {
        visit_task(tasks[dependency.0.as_str()], tasks, visiting, visited, path)?;
    }
    visiting.remove(id);
    visited.insert(id);
    Ok(())
}

#[cfg(test)]
mod tests {
    use winwincode_domain::DeliveryTaskId;

    use crate::domain::{AcceptanceCriterionId, Delivery, test_fixture};

    #[test]
    fn delivery_task_requires_current_acceptance_criteria() {
        let mut fixture = test_fixture();
        fixture.tasks[0].acceptance_criterion_ids = vec![AcceptanceCriterionId("foreign".into())];
        assert!(Delivery::try_from_snapshot(fixture).is_err());
    }

    #[test]
    fn delivery_task_rejects_missing_dependency() {
        let mut fixture = test_fixture();
        fixture.tasks[0].blocked_by_task_ids = vec![DeliveryTaskId("missing".into())];
        assert!(Delivery::try_from_snapshot(fixture).is_err());
    }

    #[test]
    fn delivery_task_rejects_self_dependency() {
        let mut fixture = test_fixture();
        fixture.tasks[0].blocked_by_task_ids = vec![fixture.tasks[0].id.clone()];
        assert!(Delivery::try_from_snapshot(fixture).is_err());
    }

    #[test]
    fn delivery_task_rejects_dependency_cycle() {
        let mut fixture = test_fixture();
        let mut second = fixture.tasks[0].clone();
        second.id = DeliveryTaskId("delivery-task-ui".into());
        second.blocked_by_task_ids = vec![fixture.tasks[0].id.clone()];
        fixture.tasks[0].blocked_by_task_ids = vec![second.id.clone()];
        fixture.tasks.push(second);
        assert!(Delivery::try_from_snapshot(fixture).is_err());
    }
}
