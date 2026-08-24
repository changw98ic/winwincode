// SPDX-License-Identifier: Apache-2.0

use std::collections::HashSet;

use serde::{Deserialize, Serialize};
use winwincode_domain::DeliveryId;

use super::{
    AcceptanceCriterionId, DeliverySpecId, DeliveryValidationError, DeliveryValidationErrorCode,
    MAX_DELIVERY_REWORK_ATTEMPTS, MAX_REFERENCE_LENGTH, MAX_TEXT_LENGTH, bounded_text,
    collection_length, duplicate_ids, portable_identifier, positive, safe_non_negative,
    schema_version, validation_error,
};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum RepositoryKind {
    #[serde(rename = "local-git")]
    LocalGit,
    #[serde(rename = "github")]
    GitHub,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct RepositoryRef {
    pub schema_version: u8,
    pub kind: RepositoryKind,
    pub locator: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct GitHubIssueSourceRef {
    pub schema_version: u8,
    pub provider: String,
    pub kind: String,
    pub repository: String,
    pub number: u64,
}

pub type DeliverySourceRef = GitHubIssueSourceRef;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct GitHubPullRequestTargetRef {
    pub schema_version: u8,
    pub provider: String,
    pub kind: String,
    pub repository: String,
    pub base_branch: String,
    pub head_repository: String,
    pub head_branch: String,
}

pub type DeliveryPublicationTarget = GitHubPullRequestTargetRef;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct AcceptanceCriterion {
    pub schema_version: u8,
    pub id: AcceptanceCriterionId,
    pub description: String,
    #[serde(deserialize_with = "super::deserialize_required_option")]
    pub verification_method: Option<String>,
    pub required: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct DeliverySpec {
    pub schema_version: u8,
    pub id: DeliverySpecId,
    pub delivery_id: DeliveryId,
    pub revision: u64,
    pub title: String,
    pub goal: String,
    pub scope: Vec<String>,
    pub out_of_scope: Vec<String>,
    pub constraints: Vec<String>,
    pub acceptance_criteria: Vec<AcceptanceCriterion>,
    #[serde(deserialize_with = "super::deserialize_required_option")]
    pub source_ref: Option<DeliverySourceRef>,
    #[serde(deserialize_with = "super::deserialize_required_option")]
    pub publication_target: Option<DeliveryPublicationTarget>,
    pub repository: RepositoryRef,
    pub base_revision: String,
    pub max_rework_attempts: u64,
    pub created_at_millis: u64,
}

pub(crate) fn validate(spec: &mut DeliverySpec, path: &str) -> Result<(), DeliveryValidationError> {
    schema_version(spec.schema_version, &format!("{path}.schemaVersion"))?;
    portable_identifier(&spec.id.0, &format!("{path}.id"))?;
    portable_identifier(&spec.delivery_id.0, &format!("{path}.deliveryId"))?;
    positive(spec.revision, &format!("{path}.revision"))?;
    bounded_text(&spec.title, &format!("{path}.title"), 256)?;
    bounded_text(&spec.goal, &format!("{path}.goal"), MAX_TEXT_LENGTH)?;
    validate_unique_texts(&spec.scope, &format!("{path}.scope"), true)?;
    validate_unique_texts(&spec.out_of_scope, &format!("{path}.outOfScope"), false)?;
    validate_unique_texts(&spec.constraints, &format!("{path}.constraints"), false)?;
    collection_length(
        spec.acceptance_criteria.len(),
        &format!("{path}.acceptanceCriteria"),
    )?;
    if spec.acceptance_criteria.is_empty()
        || !spec
            .acceptance_criteria
            .iter()
            .any(|criterion| criterion.required)
    {
        return Err(validation_error(
            DeliveryValidationErrorCode::InvalidValue,
            format!("{path}.acceptanceCriteria"),
            "delivery spec must contain at least one required acceptance criterion",
        ));
    }
    for (index, criterion) in spec.acceptance_criteria.iter().enumerate() {
        let criterion_path = format!("{path}.acceptanceCriteria[{index}]");
        schema_version(
            criterion.schema_version,
            &format!("{criterion_path}.schemaVersion"),
        )?;
        portable_identifier(&criterion.id.0, &format!("{criterion_path}.id"))?;
        bounded_text(
            &criterion.description,
            &format!("{criterion_path}.description"),
            MAX_TEXT_LENGTH,
        )?;
        if let Some(method) = &criterion.verification_method {
            bounded_text(
                method,
                &format!("{criterion_path}.verificationMethod"),
                MAX_TEXT_LENGTH,
            )?;
        }
    }
    duplicate_ids(
        spec.acceptance_criteria
            .iter()
            .map(|criterion| criterion.id.0.as_str()),
        &format!("{path}.acceptanceCriteria"),
    )?;
    validate_repository(&spec.repository, &format!("{path}.repository"))?;
    bounded_text(
        &spec.base_revision,
        &format!("{path}.baseRevision"),
        MAX_REFERENCE_LENGTH,
    )?;
    safe_non_negative(
        spec.max_rework_attempts,
        &format!("{path}.maxReworkAttempts"),
    )?;
    if spec.max_rework_attempts > MAX_DELIVERY_REWORK_ATTEMPTS {
        return Err(validation_error(
            DeliveryValidationErrorCode::InvalidValue,
            format!("{path}.maxReworkAttempts"),
            format!("must be at most {MAX_DELIVERY_REWORK_ATTEMPTS}"),
        ));
    }
    safe_non_negative(spec.created_at_millis, &format!("{path}.createdAtMillis"))?;

    if let Some(source) = &mut spec.source_ref {
        validate_source_ref(source, &format!("{path}.sourceRef"))?;
        let expected = format!("github-issue:{}:{}", source.repository, source.number);
        if spec.delivery_id.0 != expected {
            return Err(validation_error(
                DeliveryValidationErrorCode::RelationshipMismatch,
                format!("{path}.deliveryId"),
                "a GitHub issue source must use its deterministic Delivery identity",
            ));
        }
    }
    if let Some(target) = &mut spec.publication_target {
        validate_publication_target(target, &format!("{path}.publicationTarget"))?;
        if spec.source_ref.is_none() {
            return Err(validation_error(
                DeliveryValidationErrorCode::RelationshipMismatch,
                format!("{path}.publicationTarget"),
                "a GitHub pull-request target requires a GitHub issue source",
            ));
        }
    }
    Ok(())
}

fn validate_unique_texts(
    values: &[String],
    path: &str,
    required: bool,
) -> Result<(), DeliveryValidationError> {
    collection_length(values.len(), path)?;
    if required && values.is_empty() {
        return Err(validation_error(
            DeliveryValidationErrorCode::InvalidValue,
            path,
            "must not be empty",
        ));
    }
    let mut unique = HashSet::with_capacity(values.len());
    for (index, value) in values.iter().enumerate() {
        bounded_text(value, &format!("{path}[{index}]"), MAX_TEXT_LENGTH)?;
        if !unique.insert(value.as_str()) {
            return Err(validation_error(
                DeliveryValidationErrorCode::DuplicateId,
                path,
                "contains duplicate entries",
            ));
        }
    }
    Ok(())
}

fn validate_repository(
    repository: &RepositoryRef,
    path: &str,
) -> Result<(), DeliveryValidationError> {
    schema_version(repository.schema_version, &format!("{path}.schemaVersion"))?;
    bounded_text(
        &repository.locator,
        &format!("{path}.locator"),
        MAX_REFERENCE_LENGTH,
    )
}

fn validate_source_ref(
    source: &mut GitHubIssueSourceRef,
    path: &str,
) -> Result<(), DeliveryValidationError> {
    schema_version(source.schema_version, &format!("{path}.schemaVersion"))?;
    if source.provider != "github" || source.kind != "issue" {
        return Err(validation_error(
            DeliveryValidationErrorCode::InvalidValue,
            path,
            "must identify a GitHub issue",
        ));
    }
    validate_github_repository(&source.repository, &format!("{path}.repository"))?;
    source.repository.make_ascii_lowercase();
    positive(source.number, &format!("{path}.number"))
}

fn validate_publication_target(
    target: &mut GitHubPullRequestTargetRef,
    path: &str,
) -> Result<(), DeliveryValidationError> {
    schema_version(target.schema_version, &format!("{path}.schemaVersion"))?;
    if target.provider != "github" || target.kind != "pull-request" {
        return Err(validation_error(
            DeliveryValidationErrorCode::InvalidValue,
            path,
            "must identify a GitHub pull-request target",
        ));
    }
    validate_github_repository(&target.repository, &format!("{path}.repository"))?;
    validate_github_repository(&target.head_repository, &format!("{path}.headRepository"))?;
    target.repository.make_ascii_lowercase();
    target.head_repository.make_ascii_lowercase();
    validate_git_branch(&target.base_branch, &format!("{path}.baseBranch"))?;
    validate_git_branch(&target.head_branch, &format!("{path}.headBranch"))?;
    if target.repository == target.head_repository && target.base_branch == target.head_branch {
        return Err(validation_error(
            DeliveryValidationErrorCode::InvalidValue,
            path,
            "GitHub pull-request base and head must identify different branches",
        ));
    }
    Ok(())
}

fn validate_github_repository(value: &str, path: &str) -> Result<(), DeliveryValidationError> {
    let Some((owner, repository)) = value.split_once('/') else {
        return Err(validation_error(
            DeliveryValidationErrorCode::InvalidValue,
            path,
            "must be a GitHub owner/repository name",
        ));
    };
    let owner_valid = !owner.is_empty()
        && owner.len() <= 39
        && owner
            .bytes()
            .next()
            .is_some_and(|byte| byte.is_ascii_alphanumeric())
        && owner
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-');
    let repository_valid = !repository.is_empty()
        && repository.len() <= 100
        && repository
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'_' | b'-'));
    if owner_valid && repository_valid && !repository.contains('/') {
        Ok(())
    } else {
        Err(validation_error(
            DeliveryValidationErrorCode::InvalidValue,
            path,
            "must be a GitHub owner/repository name",
        ))
    }
}

fn validate_git_branch(value: &str, path: &str) -> Result<(), DeliveryValidationError> {
    let segments_valid = value.split('/').all(|segment| {
        !segment.is_empty() && !segment.starts_with('.') && !segment.as_bytes().ends_with(b".lock")
    });
    let invalid_character = value.chars().any(|character| {
        let code = u32::from(character);
        code <= 0x20
            || code == 0x7f
            || matches!(character, '~' | '^' | ':' | '?' | '*' | '\\' | '[')
    });
    let valid = !value.is_empty()
        && value.encode_utf16().count() <= 255
        && value != "@"
        && !value.starts_with('/')
        && !value.ends_with('/')
        && !value.ends_with('.')
        && !value.contains("..")
        && !value.contains("@{")
        && !value.contains("//")
        && !invalid_character
        && segments_valid;
    if valid {
        Ok(())
    } else {
        Err(validation_error(
            DeliveryValidationErrorCode::InvalidValue,
            path,
            "must be a valid Git branch name",
        ))
    }
}

#[cfg(test)]
mod tests {
    use crate::domain::{Delivery, DeliveryValidationErrorCode, test_fixture};

    #[test]
    fn delivery_spec_requires_at_least_one_required_acceptance_criterion() {
        let mut fixture = test_fixture();
        for criterion in &mut fixture.spec.acceptance_criteria {
            criterion.required = false;
        }
        assert_eq!(
            Delivery::try_from_snapshot(fixture)
                .expect_err("must fail")
                .code(),
            DeliveryValidationErrorCode::InvalidValue
        );
    }
}
