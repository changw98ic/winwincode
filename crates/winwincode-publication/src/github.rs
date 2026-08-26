// SPDX-License-Identifier: Apache-2.0

//! GitHub REST adapter for the durable publication provider port.

use std::fmt;
use std::str::FromStr;
use std::time::Duration;

use serde_json::Value;
use winwincode_domain::CredentialReferenceId;

use crate::facts::{canonical_prefixed_id, portable};
use crate::operation::{
    PublicationOperation, PublicationOperationPayload, PublicationPort, PublicationPortError,
    PublicationPortMutation, PublicationPortObservation,
};
use crate::{PublicationResourceFact, PublicationResourceKind};

const DEFAULT_GITHUB_API_VERSION: &str = "2022-11-28";
const DEFAULT_TIMEOUT_MILLIS: u64 = 30_000;
const DEFAULT_MAX_LOOKUP_PAGES: u16 = 100;
const MAX_RESPONSE_BYTES: u64 = 2 * 1_024 * 1_024;
const PAGE_SIZE: usize = 100;
const USER_AGENT: &str = "WinWinCode-GitHub-Publication";

/// Secret material resolved for one provider request.
///
/// The value is intentionally neither serializable nor cloneable. Its `Debug` output is redacted,
/// and the owned bytes are overwritten when the value is dropped.
pub struct GitHubCredential {
    provider_id: String,
    secret: Vec<u8>,
}

impl GitHubCredential {
    /// Creates one short-lived in-memory credential supplied by the Control Plane credential owner.
    ///
    /// # Errors
    ///
    /// Rejects a non-portable provider identifier or a token that cannot be used in an HTTP header.
    pub fn try_new(
        provider_id: impl Into<String>,
        secret: impl AsRef<[u8]>,
    ) -> Result<Self, String> {
        let provider_id = provider_id.into();
        let secret = secret.as_ref();
        if !portable(&provider_id, 128)
            || secret.is_empty()
            || secret.len() > 4_096
            || !secret.iter().all(|byte| matches!(byte, 0x21..=0x7e))
        {
            return Err("resolved GitHub credential is invalid".to_owned());
        }
        Ok(Self {
            provider_id,
            secret: secret.to_vec(),
        })
    }

    fn token(&self) -> &str {
        // `try_new` only accepts visible ASCII.
        std::str::from_utf8(&self.secret).expect("validated visible ASCII credential")
    }
}

impl fmt::Debug for GitHubCredential {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("GitHubCredential")
            .field("provider_id", &self.provider_id)
            .field("secret", &"[REDACTED]")
            .finish()
    }
}

impl Drop for GitHubCredential {
    fn drop(&mut self) {
        self.secret.fill(0);
    }
}

/// Stable, secret-safe failure returned by the Control Plane credential interface.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CredentialResolutionError {
    kind: CredentialResolutionErrorKind,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum CredentialResolutionErrorKind {
    NotConfigured,
    PermissionDenied,
    Unavailable,
}

impl CredentialResolutionError {
    #[must_use]
    pub const fn not_configured() -> Self {
        Self {
            kind: CredentialResolutionErrorKind::NotConfigured,
        }
    }

    #[must_use]
    pub const fn permission_denied() -> Self {
        Self {
            kind: CredentialResolutionErrorKind::PermissionDenied,
        }
    }

    #[must_use]
    pub const fn unavailable() -> Self {
        Self {
            kind: CredentialResolutionErrorKind::Unavailable,
        }
    }

    #[must_use]
    pub const fn code(&self) -> &'static str {
        match self.kind {
            CredentialResolutionErrorKind::NotConfigured => "credential-not-configured",
            CredentialResolutionErrorKind::PermissionDenied => "credential-resolution-denied",
            CredentialResolutionErrorKind::Unavailable => "credential-resolution-unavailable",
        }
    }
}

impl fmt::Display for CredentialResolutionError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("GitHub credential could not be resolved")
    }
}

impl std::error::Error for CredentialResolutionError {}

/// Control Plane-owned seam that resolves a credential reference for each individual request.
pub trait GitHubCredentialResolver {
    /// Resolves one credential reference into short-lived memory.
    ///
    /// # Errors
    ///
    /// Returns a secret-safe error when the reference is missing or temporarily unavailable.
    fn resolve(
        &mut self,
        reference: &CredentialReferenceId,
    ) -> Result<GitHubCredential, CredentialResolutionError>;
}

/// Safe GitHub adapter configuration. It contains a reference, never credential material.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GitHubAdapterConfig {
    credential_reference_id: CredentialReferenceId,
    api_base_url: String,
    api_version: String,
    request_timeout_millis: u64,
    max_lookup_pages: u16,
}

impl GitHubAdapterConfig {
    /// Creates the canonical adapter configuration with production-safe defaults.
    ///
    /// Plain HTTP is accepted only for loopback test servers.
    ///
    /// # Errors
    ///
    /// Rejects an invalid credential reference or a credential-bearing/non-HTTPS remote URL.
    pub fn try_new(
        credential_reference_id: CredentialReferenceId,
        api_base_url: impl Into<String>,
    ) -> Result<Self, String> {
        if !canonical_prefixed_id(&credential_reference_id.0, "crd_") {
            return Err("GitHub credential reference is invalid".to_owned());
        }
        let api_base_url = canonical_api_base_url(&api_base_url.into())?;
        Ok(Self {
            credential_reference_id,
            api_base_url,
            api_version: DEFAULT_GITHUB_API_VERSION.to_owned(),
            request_timeout_millis: DEFAULT_TIMEOUT_MILLIS,
            max_lookup_pages: DEFAULT_MAX_LOOKUP_PAGES,
        })
    }
}

struct GitHubResponse {
    status: u16,
    rate_limited: bool,
    body: Option<Value>,
}

/// Blocking Rust GitHub REST implementation of the canonical publication port.
pub struct GitHubPublicationAdapter<Resolver> {
    config: GitHubAdapterConfig,
    credentials: Resolver,
    agent: ureq::Agent,
}

impl<Resolver> GitHubPublicationAdapter<Resolver> {
    #[must_use]
    pub fn new(config: GitHubAdapterConfig, credentials: Resolver) -> Self {
        let agent: ureq::Agent = ureq::Agent::config_builder()
            .http_status_as_error(false)
            .max_redirects(0)
            .timeout_global(Some(Duration::from_millis(config.request_timeout_millis)))
            .build()
            .into();
        Self {
            config,
            credentials,
            agent,
        }
    }

    #[must_use]
    pub fn into_credential_resolver(self) -> Resolver {
        self.credentials
    }
}

impl<Resolver: GitHubCredentialResolver> GitHubPublicationAdapter<Resolver> {
    fn request(
        &mut self,
        method: &str,
        path: &str,
        body: Option<&Value>,
    ) -> Result<GitHubResponse, PublicationPortError> {
        let credential = self
            .credentials
            .resolve(&self.config.credential_reference_id)
            .map_err(|error| port_error(error.code()))?;
        if credential.provider_id != "github" {
            return Err(port_error("credential-provider-mismatch"));
        }
        let url = format!(
            "{}{}",
            self.config.api_base_url,
            path.trim_start_matches('/')
        );
        let authorization = format!("Bearer {}", credential.token());
        let response = match (method, body) {
            ("GET", None) => self
                .agent
                .get(&url)
                .header("Accept", "application/vnd.github+json")
                .header("Authorization", &authorization)
                .header("User-Agent", USER_AGENT)
                .header("X-GitHub-Api-Version", &self.config.api_version)
                .call(),
            ("POST", Some(value)) => self
                .agent
                .post(&url)
                .header("Accept", "application/vnd.github+json")
                .header("Authorization", &authorization)
                .header("User-Agent", USER_AGENT)
                .header("X-GitHub-Api-Version", &self.config.api_version)
                .send_json(value),
            _ => return Err(port_error("github-request-invalid")),
        }
        .map_err(|_| port_error("github-transport-unknown"))?;
        let status = response.status().as_u16();
        let rate_limited = status == 429
            || status == 403
                && (response
                    .headers()
                    .get("x-ratelimit-remaining")
                    .is_some_and(|value| value == "0")
                    || response.headers().contains_key("retry-after"));
        let bytes = response
            .into_body()
            .with_config()
            .limit(MAX_RESPONSE_BYTES)
            .read_to_vec()
            .map_err(|_| port_error("github-response-unreadable"))?;
        let body = if bytes.is_empty() {
            None
        } else {
            serde_json::from_slice(&bytes).ok()
        };
        Ok(GitHubResponse {
            status,
            rate_limited,
            body,
        })
    }

    fn lookup_branch(
        &mut self,
        operation: &PublicationOperation,
        repository: &str,
        branch: &str,
        commit_id: &str,
    ) -> Result<PublicationPortObservation, PublicationPortError> {
        let response = self.request(
            "GET",
            &format!(
                "repos/{}/git/ref/heads/{}",
                encode_repository(repository),
                encode_branch(branch)
            ),
            None,
        )?;
        if response.status == 404 {
            return Ok(PublicationPortObservation::absent(operation));
        }
        if response.status != 200 {
            return Ok(observation_for_response(operation, &response));
        }
        let sha = response
            .body
            .as_ref()
            .and_then(Value::as_object)
            .and_then(|value| value.get("object"))
            .and_then(Value::as_object)
            .and_then(|value| value.get("sha"))
            .and_then(Value::as_str);
        Ok(match sha {
            Some(value) if value == commit_id => {
                PublicationPortObservation::found(operation, operation.request_sha256(), None)
            }
            Some(_) => PublicationPortObservation::conflict(operation, "branch-ref-conflict"),
            None => PublicationPortObservation::unknown(operation, "github-response-invalid"),
        })
    }

    fn apply_branch(
        &mut self,
        operation: &PublicationOperation,
        repository: &str,
        branch: &str,
        commit_id: &str,
    ) -> Result<PublicationPortMutation, PublicationPortError> {
        let response = self.request(
            "POST",
            &format!("repos/{}/git/refs", encode_repository(repository)),
            Some(&serde_json::json!({
                "ref": format!("refs/heads/{branch}"),
                "sha": commit_id,
            })),
        )?;
        if response.status == 201 {
            let sha = response
                .body
                .as_ref()
                .and_then(Value::as_object)
                .and_then(|value| value.get("object"))
                .and_then(Value::as_object)
                .and_then(|value| value.get("sha"))
                .and_then(Value::as_str);
            return Ok(match sha {
                Some(value) if value == commit_id => {
                    PublicationPortMutation::applied(operation, None, true)
                }
                _ => PublicationPortMutation::unknown(operation, "github-response-invalid"),
            });
        }
        if matches!(response.status, 409 | 422) {
            return Ok(mutation_from_observation(
                operation,
                self.lookup_branch(operation, repository, branch, commit_id)?,
            ));
        }
        Ok(mutation_for_response(operation, &response))
    }

    #[allow(clippy::too_many_arguments)]
    fn lookup_pull_request(
        &mut self,
        operation: &PublicationOperation,
        repository: &str,
        base_branch: &str,
        head_repository: &str,
        head_branch: &str,
        title: &str,
        body: &str,
    ) -> Result<PublicationPortObservation, PublicationPortError> {
        if !body.contains(&marker(operation)) {
            return Ok(PublicationPortObservation::conflict(
                operation,
                "pull-request-marker-missing",
            ));
        }
        let head_owner = head_repository
            .split_once('/')
            .map_or(head_repository, |(owner, _)| owner);
        for page in 1..=self.config.max_lookup_pages {
            let path = format!(
                "repos/{}/pulls?state=all&head={}&base={}&per_page={PAGE_SIZE}&page={page}",
                encode_repository(repository),
                encode_query_value(&format!("{head_owner}:{head_branch}")),
                encode_query_value(base_branch),
            );
            let response = self.request("GET", &path, None)?;
            if response.status != 200 {
                return Ok(observation_for_response(operation, &response));
            }
            let Some(entries) = response.body.as_ref().and_then(Value::as_array) else {
                return Ok(PublicationPortObservation::unknown(
                    operation,
                    "github-response-invalid",
                ));
            };
            for entry in entries {
                match pull_request_match(
                    entry,
                    repository,
                    base_branch,
                    head_repository,
                    head_branch,
                    title,
                    body,
                ) {
                    RemoteMatch::Current(number) => {
                        return Ok(PublicationPortObservation::found(
                            operation,
                            operation.request_sha256(),
                            Some(pull_request_resource(repository, number)?),
                        ));
                    }
                    RemoteMatch::Conflict => {
                        return Ok(PublicationPortObservation::conflict(
                            operation,
                            "pull-request-conflict",
                        ));
                    }
                    RemoteMatch::Invalid => {
                        return Ok(PublicationPortObservation::unknown(
                            operation,
                            "github-response-invalid",
                        ));
                    }
                    RemoteMatch::Unrelated => {}
                }
            }
            if entries.len() < PAGE_SIZE {
                return Ok(PublicationPortObservation::absent(operation));
            }
        }
        Ok(PublicationPortObservation::unknown(
            operation,
            "lookup-capacity-exceeded",
        ))
    }

    #[allow(clippy::too_many_arguments)]
    fn apply_pull_request(
        &mut self,
        operation: &PublicationOperation,
        repository: &str,
        base_branch: &str,
        head_repository: &str,
        head_branch: &str,
        title: &str,
        body: &str,
    ) -> Result<PublicationPortMutation, PublicationPortError> {
        if !body.contains(&marker(operation)) {
            return Ok(PublicationPortMutation::rejected(
                operation,
                "pull-request-marker-missing",
            ));
        }
        let (head_owner, head_name) = head_repository
            .split_once('/')
            .expect("validated repository contains owner and name");
        let base_owner = repository
            .split_once('/')
            .map_or(repository, |(owner, _)| owner);
        let cross_repository = !same_repository(repository, head_repository);
        let mut request = serde_json::Map::from_iter([
            ("title".to_owned(), Value::String(title.to_owned())),
            ("body".to_owned(), Value::String(body.to_owned())),
            (
                "head".to_owned(),
                Value::String(if cross_repository {
                    format!("{head_owner}:{head_branch}")
                } else {
                    head_branch.to_owned()
                }),
            ),
            ("base".to_owned(), Value::String(base_branch.to_owned())),
        ]);
        if cross_repository && head_owner.eq_ignore_ascii_case(base_owner) {
            request.insert("head_repo".to_owned(), Value::String(head_name.to_owned()));
        }
        let response = self.request(
            "POST",
            &format!("repos/{}/pulls", encode_repository(repository)),
            Some(&Value::Object(request)),
        )?;
        if response.status == 201 {
            return Ok(
                match response.body.as_ref().map(|entry| {
                    pull_request_match(
                        entry,
                        repository,
                        base_branch,
                        head_repository,
                        head_branch,
                        title,
                        body,
                    )
                }) {
                    Some(RemoteMatch::Current(number)) => PublicationPortMutation::applied(
                        operation,
                        Some(pull_request_resource(repository, number)?),
                        true,
                    ),
                    _ => PublicationPortMutation::unknown(operation, "github-response-invalid"),
                },
            );
        }
        if matches!(response.status, 409 | 422) {
            return Ok(mutation_from_observation(
                operation,
                self.lookup_pull_request(
                    operation,
                    repository,
                    base_branch,
                    head_repository,
                    head_branch,
                    title,
                    body,
                )?,
            ));
        }
        Ok(mutation_for_response(operation, &response))
    }

    fn lookup_issue_comment(
        &mut self,
        operation: &PublicationOperation,
        repository: &str,
        issue_number: u64,
        body: &str,
    ) -> Result<PublicationPortObservation, PublicationPortError> {
        if !body.contains(&marker(operation)) {
            return Ok(PublicationPortObservation::conflict(
                operation,
                "issue-comment-marker-missing",
            ));
        }
        for page in 1..=self.config.max_lookup_pages {
            let path = format!(
                "repos/{}/issues/{issue_number}/comments?per_page={PAGE_SIZE}&page={page}",
                encode_repository(repository),
            );
            let response = self.request("GET", &path, None)?;
            if response.status != 200 {
                return Ok(observation_for_response(operation, &response));
            }
            let Some(entries) = response.body.as_ref().and_then(Value::as_array) else {
                return Ok(PublicationPortObservation::unknown(
                    operation,
                    "github-response-invalid",
                ));
            };
            for entry in entries {
                let Some(remote_body) = entry.get("body").and_then(Value::as_str) else {
                    return Ok(PublicationPortObservation::unknown(
                        operation,
                        "github-response-invalid",
                    ));
                };
                if !remote_body.contains(&marker(operation)) {
                    continue;
                }
                return Ok(if remote_body == body {
                    PublicationPortObservation::found(operation, operation.request_sha256(), None)
                } else {
                    PublicationPortObservation::conflict(operation, "issue-comment-conflict")
                });
            }
            if entries.len() < PAGE_SIZE {
                return Ok(PublicationPortObservation::absent(operation));
            }
        }
        Ok(PublicationPortObservation::unknown(
            operation,
            "lookup-capacity-exceeded",
        ))
    }

    fn apply_issue_comment(
        &mut self,
        operation: &PublicationOperation,
        repository: &str,
        issue_number: u64,
        body: &str,
    ) -> Result<PublicationPortMutation, PublicationPortError> {
        if !body.contains(&marker(operation)) {
            return Ok(PublicationPortMutation::rejected(
                operation,
                "issue-comment-marker-missing",
            ));
        }
        let response = self.request(
            "POST",
            &format!(
                "repos/{}/issues/{issue_number}/comments",
                encode_repository(repository)
            ),
            Some(&serde_json::json!({ "body": body })),
        )?;
        if response.status == 201 {
            let matches = response
                .body
                .as_ref()
                .and_then(|entry| entry.get("body"))
                .and_then(Value::as_str)
                == Some(body);
            return Ok(if matches {
                PublicationPortMutation::applied(operation, None, true)
            } else {
                PublicationPortMutation::unknown(operation, "github-response-invalid")
            });
        }
        Ok(mutation_for_response(operation, &response))
    }

    #[allow(clippy::too_many_arguments)]
    fn lookup_commit_status(
        &mut self,
        operation: &PublicationOperation,
        repository: &str,
        commit_id: &str,
        context: &str,
        state: &str,
        description: &str,
        target_repository: &str,
        target_issue_number: u64,
    ) -> Result<PublicationPortObservation, PublicationPortError> {
        let expected_target = issue_url(target_repository, target_issue_number);
        for page in 1..=self.config.max_lookup_pages {
            let path = format!(
                "repos/{}/commits/{}/statuses?per_page={PAGE_SIZE}&page={page}",
                encode_repository(repository),
                encode_path_segment(commit_id),
            );
            let response = self.request("GET", &path, None)?;
            if response.status != 200 {
                return Ok(observation_for_response(operation, &response));
            }
            let Some(entries) = response.body.as_ref().and_then(Value::as_array) else {
                return Ok(PublicationPortObservation::unknown(
                    operation,
                    "github-response-invalid",
                ));
            };
            for entry in entries {
                let Some(remote_context) = entry.get("context").and_then(Value::as_str) else {
                    return Ok(PublicationPortObservation::unknown(
                        operation,
                        "github-response-invalid",
                    ));
                };
                if remote_context != context {
                    continue;
                }
                let current = entry.get("state").and_then(Value::as_str) == Some(state)
                    && entry.get("description").and_then(Value::as_str) == Some(description)
                    && entry.get("target_url").and_then(Value::as_str)
                        == Some(expected_target.as_str());
                return Ok(if current {
                    PublicationPortObservation::found(operation, operation.request_sha256(), None)
                } else {
                    // GitHub statuses are append-only; a different status under the same context
                    // does not own the current request and a new exact status may be created.
                    PublicationPortObservation::absent(operation)
                });
            }
            if entries.len() < PAGE_SIZE {
                return Ok(PublicationPortObservation::absent(operation));
            }
        }
        Ok(PublicationPortObservation::unknown(
            operation,
            "lookup-capacity-exceeded",
        ))
    }

    #[allow(clippy::too_many_arguments)]
    fn apply_commit_status(
        &mut self,
        operation: &PublicationOperation,
        repository: &str,
        commit_id: &str,
        context: &str,
        state: &str,
        description: &str,
        target_repository: &str,
        target_issue_number: u64,
    ) -> Result<PublicationPortMutation, PublicationPortError> {
        let target_url = issue_url(target_repository, target_issue_number);
        let response = self.request(
            "POST",
            &format!(
                "repos/{}/statuses/{}",
                encode_repository(repository),
                encode_path_segment(commit_id)
            ),
            Some(&serde_json::json!({
                "state": state,
                "target_url": target_url,
                "description": description,
                "context": context,
            })),
        )?;
        if response.status == 201 {
            let current = response.body.as_ref().is_some_and(|entry| {
                entry.get("state").and_then(Value::as_str) == Some(state)
                    && entry.get("description").and_then(Value::as_str) == Some(description)
                    && entry.get("context").and_then(Value::as_str) == Some(context)
                    && entry.get("target_url").and_then(Value::as_str) == Some(target_url.as_str())
            });
            return Ok(if current {
                PublicationPortMutation::applied(operation, None, true)
            } else {
                PublicationPortMutation::unknown(operation, "github-response-invalid")
            });
        }
        Ok(mutation_for_response(operation, &response))
    }
}

impl<Resolver: GitHubCredentialResolver> PublicationPort for GitHubPublicationAdapter<Resolver> {
    fn lookup(
        &mut self,
        operation: &PublicationOperation,
    ) -> Result<PublicationPortObservation, PublicationPortError> {
        operation
            .validate()
            .map_err(|_| port_error("invalid-operation"))?;
        match operation.payload() {
            PublicationOperationPayload::Branch {
                repository,
                branch,
                commit_id,
            } => self.lookup_branch(operation, repository, branch, commit_id),
            PublicationOperationPayload::PullRequest {
                repository,
                base_branch,
                head_repository,
                head_branch,
                title,
                body,
            } => self.lookup_pull_request(
                operation,
                repository,
                base_branch,
                head_repository,
                head_branch,
                title,
                body,
            ),
            PublicationOperationPayload::IssueComment {
                repository,
                issue_number,
                body,
            } => self.lookup_issue_comment(operation, repository, *issue_number, body),
            PublicationOperationPayload::CommitStatus {
                repository,
                commit_id,
                context,
                state,
                description,
                target_repository,
                target_issue_number,
            } => self.lookup_commit_status(
                operation,
                repository,
                commit_id,
                context,
                state,
                description,
                target_repository,
                *target_issue_number,
            ),
        }
    }

    fn apply(
        &mut self,
        operation: &PublicationOperation,
    ) -> Result<PublicationPortMutation, PublicationPortError> {
        operation
            .validate()
            .map_err(|_| port_error("invalid-operation"))?;
        match operation.payload() {
            PublicationOperationPayload::Branch {
                repository,
                branch,
                commit_id,
            } => self.apply_branch(operation, repository, branch, commit_id),
            PublicationOperationPayload::PullRequest {
                repository,
                base_branch,
                head_repository,
                head_branch,
                title,
                body,
            } => self.apply_pull_request(
                operation,
                repository,
                base_branch,
                head_repository,
                head_branch,
                title,
                body,
            ),
            PublicationOperationPayload::IssueComment {
                repository,
                issue_number,
                body,
            } => self.apply_issue_comment(operation, repository, *issue_number, body),
            PublicationOperationPayload::CommitStatus {
                repository,
                commit_id,
                context,
                state,
                description,
                target_repository,
                target_issue_number,
            } => self.apply_commit_status(
                operation,
                repository,
                commit_id,
                context,
                state,
                description,
                target_repository,
                *target_issue_number,
            ),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum RemoteMatch {
    Current(u64),
    Conflict,
    Unrelated,
    Invalid,
}

#[allow(clippy::too_many_arguments)]
fn pull_request_match(
    entry: &Value,
    repository: &str,
    base_branch: &str,
    head_repository: &str,
    head_branch: &str,
    title: &str,
    body: &str,
) -> RemoteMatch {
    let Some(entry) = entry.as_object() else {
        return RemoteMatch::Invalid;
    };
    let remote_body = entry.get("body").and_then(Value::as_str).unwrap_or("");
    let head = entry.get("head").and_then(Value::as_object);
    let base = entry.get("base").and_then(Value::as_object);
    let remote_head_repository = head
        .and_then(|value| value.get("repo"))
        .and_then(Value::as_object)
        .and_then(|value| value.get("full_name"))
        .and_then(Value::as_str);
    let remote_base_repository = base
        .and_then(|value| value.get("repo"))
        .and_then(Value::as_object)
        .and_then(|value| value.get("full_name"))
        .and_then(Value::as_str);
    let same_route = head
        .and_then(|value| value.get("ref"))
        .and_then(Value::as_str)
        == Some(head_branch)
        && remote_head_repository.is_some_and(|value| same_repository(value, head_repository))
        && base
            .and_then(|value| value.get("ref"))
            .and_then(Value::as_str)
            == Some(base_branch)
        && remote_base_repository.is_some_and(|value| same_repository(value, repository));
    let expected_marker = body
        .lines()
        .find(|line| line.starts_with("<!-- winwincode-publication:"));
    let owns_marker = expected_marker.is_some_and(|value| remote_body.contains(value));
    if !owns_marker {
        if !same_route {
            return RemoteMatch::Unrelated;
        }
        return match entry.get("state").and_then(Value::as_str) {
            Some("open") => RemoteMatch::Conflict,
            Some("closed") => RemoteMatch::Unrelated,
            _ => RemoteMatch::Invalid,
        };
    }
    if !same_route
        || entry.get("title").and_then(Value::as_str) != Some(title)
        || remote_body != body
    {
        return RemoteMatch::Conflict;
    }
    entry
        .get("number")
        .and_then(Value::as_u64)
        .filter(|number| *number > 0)
        .map_or(RemoteMatch::Invalid, RemoteMatch::Current)
}

fn pull_request_resource(
    repository: &str,
    number: u64,
) -> Result<PublicationResourceFact, PublicationPortError> {
    PublicationResourceFact::try_new(
        PublicationResourceKind::GitHubPullRequest,
        repository,
        number,
    )
    .map_err(|_| port_error("github-response-invalid"))
}

fn mutation_from_observation(
    operation: &PublicationOperation,
    observation: PublicationPortObservation,
) -> PublicationPortMutation {
    match observation {
        PublicationPortObservation::Found { resource, .. } => {
            PublicationPortMutation::applied(operation, resource, false)
        }
        PublicationPortObservation::Conflict { code, .. } => {
            PublicationPortMutation::rejected(operation, code)
        }
        PublicationPortObservation::Unknown { code, .. } => {
            PublicationPortMutation::unknown(operation, code)
        }
        PublicationPortObservation::Absent { .. } => {
            PublicationPortMutation::unknown(operation, "github-create-not-confirmed")
        }
    }
}

fn observation_for_response(
    operation: &PublicationOperation,
    response: &GitHubResponse,
) -> PublicationPortObservation {
    let code = response_code(response);
    if response.rate_limited || response.status >= 500 {
        PublicationPortObservation::unknown(operation, code)
    } else {
        PublicationPortObservation::conflict(operation, code)
    }
}

fn mutation_for_response(
    operation: &PublicationOperation,
    response: &GitHubResponse,
) -> PublicationPortMutation {
    if response.rate_limited || response.status >= 500 {
        PublicationPortMutation::unknown(operation, response_code(response))
    } else {
        PublicationPortMutation::rejected(operation, response_code(response))
    }
}

fn port_error(code: &str) -> PublicationPortError {
    PublicationPortError::new(code).expect("adapter error codes are portable")
}

fn http_code(status: u16) -> String {
    format!("github-http-{status}")
}

fn response_code(response: &GitHubResponse) -> String {
    if response.rate_limited {
        "github-rate-limited".to_owned()
    } else {
        match response.status {
            401 => "github-authentication-failed".to_owned(),
            403 => "github-permission-denied".to_owned(),
            500..=599 => "github-service-unavailable".to_owned(),
            status => http_code(status),
        }
    }
}

fn marker(operation: &PublicationOperation) -> String {
    let provider_key = operation
        .operation_key()
        .rsplit_once(':')
        .map_or(operation.operation_key(), |(value, _)| value);
    format!("<!-- winwincode-publication:{provider_key} -->")
}

fn issue_url(repository: &str, issue_number: u64) -> String {
    format!("https://github.com/{repository}/issues/{issue_number}")
}

fn same_repository(left: &str, right: &str) -> bool {
    left.eq_ignore_ascii_case(right)
}

fn canonical_api_base_url(value: &str) -> Result<String, String> {
    let uri = ureq::http::Uri::from_str(value)
        .map_err(|_| "GitHub API base URL is invalid".to_owned())?;
    let scheme = uri
        .scheme_str()
        .ok_or_else(|| "GitHub API base URL is invalid".to_owned())?;
    let authority = uri
        .authority()
        .ok_or_else(|| "GitHub API base URL is invalid".to_owned())?;
    let host = authority.host();
    let loopback = matches!(host, "localhost" | "127.0.0.1" | "[::1]" | "::1");
    if authority.as_str().contains('@')
        || uri
            .path_and_query()
            .and_then(|value| value.query())
            .is_some()
        || scheme != "https" && !(scheme == "http" && loopback)
    {
        return Err(
            "GitHub API base URL must be credential-free HTTPS or loopback HTTP".to_owned(),
        );
    }
    let mut canonical = value.trim_end_matches('/').to_owned();
    canonical.push('/');
    Ok(canonical)
}

fn encode_repository(repository: &str) -> String {
    repository
        .split('/')
        .map(encode_path_segment)
        .collect::<Vec<_>>()
        .join("/")
}

fn encode_branch(branch: &str) -> String {
    branch
        .split('/')
        .map(encode_path_segment)
        .collect::<Vec<_>>()
        .join("/")
}

fn encode_query_value(value: &str) -> String {
    encode_path_segment(value)
}

fn encode_path_segment(value: &str) -> String {
    let mut encoded = String::new();
    for byte in value.bytes() {
        if byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'.' | b'_' | b'~') {
            encoded.push(char::from(byte));
        } else {
            use std::fmt::Write as _;
            write!(encoded, "%{byte:02X}").expect("write to string");
        }
    }
    encoded
}
