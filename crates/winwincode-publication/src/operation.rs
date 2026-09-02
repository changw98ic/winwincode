// SPDX-License-Identifier: Apache-2.0

use std::fmt;

use serde::{Deserialize, Serialize};

use crate::facts::{
    PublicationAuthorization, PublicationResourceFact, canonical_sha256_json, git_branch,
    git_object, portable, repository_slug,
};

pub const PUBLICATION_OPERATION_SCHEMA_VERSION: u64 = 1;
pub const PUBLICATION_OPERATION_PROTOCOL: &str = "winwincode.github-provider-operation.v1";

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum PublicationOperationKind {
    Branch,
    PullRequest,
    IssueComment,
    CommitStatus,
}

impl PublicationOperationKind {
    pub(crate) const fn key(self) -> &'static str {
        match self {
            Self::Branch => "branch",
            Self::PullRequest => "pull-request",
            Self::IssueComment => "issue-comment",
            Self::CommitStatus => "commit-status",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", content = "payload", rename_all = "kebab-case")]
pub enum PublicationOperationPayload {
    Branch {
        repository: String,
        branch: String,
        commit_id: String,
    },
    PullRequest {
        repository: String,
        base_branch: String,
        head_repository: String,
        head_branch: String,
        title: String,
        body: String,
    },
    IssueComment {
        repository: String,
        issue_number: u64,
        body: String,
    },
    CommitStatus {
        repository: String,
        commit_id: String,
        context: String,
        state: String,
        description: String,
        target_repository: String,
        target_issue_number: u64,
    },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct PublicationOperation {
    schema_version: u64,
    protocol: String,
    kind: PublicationOperationKind,
    operation_key: String,
    request_sha256: String,
    payload: PublicationOperationPayload,
}

impl PublicationOperation {
    pub(crate) fn ordered(authorization: &PublicationAuthorization) -> Vec<Self> {
        let key = &authorization.provider_idempotency_key;
        let target = &authorization.target;
        let source = &authorization.source;
        let package_id = format!(
            "github-review-package:{}",
            authorization.publication_set_sha256.0
        );
        let title = format!(
            "WinWinCode Delivery {}",
            authorization.binding.delivery_id().0
        );
        let marker = format!("<!-- winwincode-publication:{key} -->");
        let body = [
            marker.clone(),
            format!(
                "Publication set: `{}`",
                authorization.publication_set_sha256.0
            ),
            format!("Candidate: `{}`", authorization.binding.candidate_ref()),
            format!("Verdict: `{}`", authorization.binding.verdict_id()),
            format!("Review package: `{package_id}`"),
        ]
        .join("\n\n");
        let comment = format!(
            "{marker}\n\nDelivery `{}` has an approved publication set `{}`.",
            authorization.binding.delivery_id().0,
            authorization.publication_set_sha256.0
        );
        vec![
            Self::new(
                key,
                PublicationOperationKind::Branch,
                PublicationOperationPayload::Branch {
                    repository: target.head_repository().to_owned(),
                    branch: target.head_branch().to_owned(),
                    commit_id: authorization.candidate_commit_id.clone(),
                },
            ),
            Self::new(
                key,
                PublicationOperationKind::PullRequest,
                PublicationOperationPayload::PullRequest {
                    repository: target.repository().to_owned(),
                    base_branch: target.base_branch().to_owned(),
                    head_repository: target.head_repository().to_owned(),
                    head_branch: target.head_branch().to_owned(),
                    title,
                    body,
                },
            ),
            Self::new(
                key,
                PublicationOperationKind::IssueComment,
                PublicationOperationPayload::IssueComment {
                    repository: source.repository().to_owned(),
                    issue_number: source.number(),
                    body: comment,
                },
            ),
            Self::new(
                key,
                PublicationOperationKind::CommitStatus,
                PublicationOperationPayload::CommitStatus {
                    repository: target.head_repository().to_owned(),
                    commit_id: authorization.candidate_commit_id.clone(),
                    context: "winwincode/delivery".to_owned(),
                    state: "success".to_owned(),
                    description: "WinWinCode verified all required acceptance criteria.".to_owned(),
                    target_repository: source.repository().to_owned(),
                    target_issue_number: source.number(),
                },
            ),
        ]
    }

    fn new(
        provider_key: &str,
        kind: PublicationOperationKind,
        payload: PublicationOperationPayload,
    ) -> Self {
        let operation_key = format!("{provider_key}:{}", kind.key());
        let request_sha256 = canonical_sha256_json(&(
            PUBLICATION_OPERATION_SCHEMA_VERSION,
            PUBLICATION_OPERATION_PROTOCOL,
            kind,
            &operation_key,
            &payload,
        ))
        .0;
        Self {
            schema_version: PUBLICATION_OPERATION_SCHEMA_VERSION,
            protocol: PUBLICATION_OPERATION_PROTOCOL.to_owned(),
            kind,
            operation_key,
            request_sha256,
            payload,
        }
    }

    pub(crate) fn validate(&self) -> Result<(), String> {
        if self.schema_version != PUBLICATION_OPERATION_SCHEMA_VERSION
            || self.protocol != PUBLICATION_OPERATION_PROTOCOL
            || !self
                .operation_key
                .ends_with(&format!(":{}", self.kind.key()))
            || self.request_sha256
                != canonical_sha256_json(&(
                    self.schema_version,
                    &self.protocol,
                    self.kind,
                    &self.operation_key,
                    &self.payload,
                ))
                .0
        {
            return Err("publication operation identity changed".to_owned());
        }
        match &self.payload {
            PublicationOperationPayload::Branch {
                repository,
                branch,
                commit_id,
            } => {
                if self.kind != PublicationOperationKind::Branch
                    || !repository_slug(repository)
                    || !git_branch(branch)
                    || !git_object(commit_id)
                {
                    return Err("publication branch operation is invalid".to_owned());
                }
            }
            PublicationOperationPayload::PullRequest {
                repository,
                base_branch,
                head_repository,
                head_branch,
                title,
                body,
            } => {
                if self.kind != PublicationOperationKind::PullRequest
                    || !repository_slug(repository)
                    || !git_branch(base_branch)
                    || !repository_slug(head_repository)
                    || !git_branch(head_branch)
                    || !bounded_text(title, 512)
                    || !bounded_text(body, 1_048_576)
                {
                    return Err("publication pull-request operation is invalid".to_owned());
                }
            }
            PublicationOperationPayload::IssueComment {
                repository,
                issue_number,
                body,
            } => {
                if self.kind != PublicationOperationKind::IssueComment
                    || !repository_slug(repository)
                    || *issue_number == 0
                    || !bounded_text(body, 1_048_576)
                {
                    return Err("publication issue-comment operation is invalid".to_owned());
                }
            }
            PublicationOperationPayload::CommitStatus {
                repository,
                commit_id,
                context,
                state,
                description,
                target_repository,
                target_issue_number,
            } => {
                if self.kind != PublicationOperationKind::CommitStatus
                    || !repository_slug(repository)
                    || !git_object(commit_id)
                    || !bounded_text(context, 255)
                    || state != "success"
                    || !bounded_text(description, 255)
                    || !repository_slug(target_repository)
                    || *target_issue_number == 0
                {
                    return Err("publication commit-status operation is invalid".to_owned());
                }
            }
        }
        Ok(())
    }

    #[must_use]
    pub const fn kind(&self) -> PublicationOperationKind {
        self.kind
    }

    #[must_use]
    pub const fn schema_version(&self) -> u64 {
        self.schema_version
    }

    #[must_use]
    pub fn protocol(&self) -> &str {
        &self.protocol
    }

    #[must_use]
    pub const fn payload(&self) -> &PublicationOperationPayload {
        &self.payload
    }

    #[must_use]
    pub fn operation_key(&self) -> &str {
        &self.operation_key
    }

    #[must_use]
    pub fn request_sha256(&self) -> &str {
        &self.request_sha256
    }
}

fn bounded_text(value: &str, maximum: usize) -> bool {
    !value.trim().is_empty()
        && value.chars().count() <= maximum
        && !value.chars().any(|character| {
            matches!(character, '\u{0000}'..='\u{0008}' | '\u{000b}' | '\u{000c}' | '\u{000e}'..='\u{001f}' | '\u{007f}')
        })
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PublicationPortObservation {
    Found {
        operation_key: String,
        request_sha256: String,
        resource: Option<PublicationResourceFact>,
    },
    Absent {
        operation_key: String,
    },
    Unknown {
        operation_key: String,
        code: String,
    },
    Conflict {
        operation_key: String,
        code: String,
    },
}

impl PublicationPortObservation {
    #[must_use]
    pub fn absent(operation: &PublicationOperation) -> Self {
        Self::Absent {
            operation_key: operation.operation_key.clone(),
        }
    }

    #[must_use]
    pub fn found(
        operation: &PublicationOperation,
        request_sha256: impl Into<String>,
        resource: Option<PublicationResourceFact>,
    ) -> Self {
        Self::Found {
            operation_key: operation.operation_key.clone(),
            request_sha256: request_sha256.into(),
            resource,
        }
    }

    #[must_use]
    pub fn unknown(operation: &PublicationOperation, code: impl Into<String>) -> Self {
        Self::Unknown {
            operation_key: operation.operation_key.clone(),
            code: code.into(),
        }
    }

    #[must_use]
    pub fn conflict(operation: &PublicationOperation, code: impl Into<String>) -> Self {
        Self::Conflict {
            operation_key: operation.operation_key.clone(),
            code: code.into(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PublicationPortMutation {
    Applied {
        operation_key: String,
        request_sha256: String,
        resource: Option<PublicationResourceFact>,
        remote_write_performed: bool,
    },
    Unknown {
        operation_key: String,
        code: String,
    },
    Rejected {
        operation_key: String,
        code: String,
    },
}

impl PublicationPortMutation {
    #[must_use]
    pub fn applied(
        operation: &PublicationOperation,
        resource: Option<PublicationResourceFact>,
        remote_write_performed: bool,
    ) -> Self {
        Self::Applied {
            operation_key: operation.operation_key.clone(),
            request_sha256: operation.request_sha256.clone(),
            resource,
            remote_write_performed,
        }
    }

    #[must_use]
    pub fn unknown(operation: &PublicationOperation, code: impl Into<String>) -> Self {
        Self::Unknown {
            operation_key: operation.operation_key.clone(),
            code: code.into(),
        }
    }

    #[must_use]
    pub fn rejected(operation: &PublicationOperation, code: impl Into<String>) -> Self {
        Self::Rejected {
            operation_key: operation.operation_key.clone(),
            code: code.into(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PublicationPortError {
    code: String,
}

impl PublicationPortError {
    /// Builds one stable provider error code without retaining raw provider text.
    ///
    /// # Errors
    ///
    /// Rejects empty, oversized, or non-portable codes.
    pub fn new(code: impl Into<String>) -> Result<Self, String> {
        let code = code.into();
        if !portable(&code, 100) {
            return Err("publication port error code is invalid".to_owned());
        }
        Ok(Self { code })
    }

    #[must_use]
    pub fn code(&self) -> &str {
        &self.code
    }
}

impl fmt::Display for PublicationPortError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("publication provider operation did not complete")
    }
}

impl std::error::Error for PublicationPortError {}

pub trait PublicationPort {
    /// Looks up the exact stable operation key before any remote write.
    ///
    /// # Errors
    ///
    /// Returns a secret-safe provider failure when lookup could not complete.
    fn lookup(
        &mut self,
        operation: &PublicationOperation,
    ) -> Result<PublicationPortObservation, PublicationPortError>;

    /// Applies one absent operation using its stable key and request digest.
    ///
    /// # Errors
    ///
    /// Returns a secret-safe provider failure when apply could not complete.
    fn apply(
        &mut self,
        operation: &PublicationOperation,
    ) -> Result<PublicationPortMutation, PublicationPortError>;
}

pub(crate) fn validate_observation(
    operation: &PublicationOperation,
    observation: &PublicationPortObservation,
) -> Result<(), String> {
    let (operation_key, code) = match observation {
        PublicationPortObservation::Found {
            operation_key,
            request_sha256,
            resource,
        } => {
            if request_sha256 != operation.request_sha256()
                || !resource_matches(operation, resource.as_ref())
            {
                return Err("publication lookup returned another request".to_owned());
            }
            (operation_key, None)
        }
        PublicationPortObservation::Absent { operation_key } => (operation_key, None),
        PublicationPortObservation::Unknown {
            operation_key,
            code,
        }
        | PublicationPortObservation::Conflict {
            operation_key,
            code,
        } => (operation_key, Some(code)),
    };
    if operation_key != operation.operation_key() || code.is_some_and(|value| !portable(value, 100))
    {
        return Err("publication lookup returned another operation".to_owned());
    }
    Ok(())
}

pub(crate) fn validate_mutation(
    operation: &PublicationOperation,
    mutation: &PublicationPortMutation,
) -> Result<(), String> {
    let (operation_key, code) = match mutation {
        PublicationPortMutation::Applied {
            operation_key,
            request_sha256,
            resource,
            ..
        } => {
            if request_sha256 != operation.request_sha256()
                || !resource_matches(operation, resource.as_ref())
            {
                return Err("publication apply returned another request".to_owned());
            }
            (operation_key, None)
        }
        PublicationPortMutation::Unknown {
            operation_key,
            code,
        }
        | PublicationPortMutation::Rejected {
            operation_key,
            code,
        } => (operation_key, Some(code)),
    };
    if operation_key != operation.operation_key() || code.is_some_and(|value| !portable(value, 100))
    {
        return Err("publication apply returned another operation".to_owned());
    }
    Ok(())
}

fn resource_matches(
    operation: &PublicationOperation,
    resource: Option<&PublicationResourceFact>,
) -> bool {
    match operation.kind() {
        PublicationOperationKind::PullRequest => resource.is_some_and(|value| {
            value.kind() == PublicationResourceKind::GitHubPullRequest && value.validate().is_ok()
        }),
        _ => resource.is_none(),
    }
}

use crate::facts::PublicationResourceKind;
