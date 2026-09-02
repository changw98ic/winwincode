// SPDX-License-Identifier: Apache-2.0

use std::fmt;
use std::net::IpAddr;

use serde::Serialize;
use sha2::{Digest, Sha256};
use winwincode_domain::{
    OrganizationId, ProjectId, PublicationId, RepositoryId, RequestId, ServiceAccountId,
    Sha256Digest, SystemActorId, UserId, WorkspaceId,
};

use crate::facts::{
    MAX_SAFE_INTEGER, PublicationAuthorization, canonical_prefixed_id, canonical_sha256,
    repository_slug,
};

/// Exact actor whose authenticated command is being evaluated.
#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
#[serde(tag = "kind", content = "id", rename_all = "snake_case")]
pub enum PublicationRequester {
    User(UserId),
    ServiceAccount(ServiceAccountId),
    System(SystemActorId),
}

impl PublicationRequester {
    fn validate(&self) -> Result<(), String> {
        let valid = match self {
            Self::User(id) => canonical_prefixed_id(&id.0, "usr_"),
            Self::ServiceAccount(id) => canonical_prefixed_id(&id.0, "svc_"),
            Self::System(id) => canonical_prefixed_id(&id.0, "sys_"),
        };
        if valid {
            Ok(())
        } else {
            Err("publication requester identity is not canonical".to_owned())
        }
    }

    fn sort_key(&self) -> String {
        match self {
            Self::User(id) => format!("user:{}", id.0),
            Self::ServiceAccount(id) => format!("service_account:{}", id.0),
            Self::System(id) => format!("system:{}", id.0),
        }
    }
}

/// Exact repository ancestry to which one policy applies.
#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct RepositoryPolicyScope {
    #[serde(rename = "organizationId")]
    organization: OrganizationId,
    #[serde(rename = "workspaceId")]
    workspace: WorkspaceId,
    #[serde(rename = "projectId")]
    project: ProjectId,
    #[serde(rename = "repositoryId")]
    repository: RepositoryId,
}

impl RepositoryPolicyScope {
    /// Builds one canonical repository policy scope.
    ///
    /// # Errors
    ///
    /// Rejects any non-canonical organization, workspace, project, or repository identity.
    pub fn try_new(
        organization_id: OrganizationId,
        workspace_id: WorkspaceId,
        project_id: ProjectId,
        repository_id: RepositoryId,
    ) -> Result<Self, String> {
        let value = Self {
            organization: organization_id,
            workspace: workspace_id,
            project: project_id,
            repository: repository_id,
        };
        value.validate()?;
        Ok(value)
    }

    fn validate(&self) -> Result<(), String> {
        if !canonical_prefixed_id(&self.organization.0, "org_")
            || !canonical_prefixed_id(&self.workspace.0, "wsp_")
            || !canonical_prefixed_id(&self.project.0, "prj_")
            || !canonical_prefixed_id(&self.repository.0, "rep_")
        {
            return Err("repository policy scope is not canonical".to_owned());
        }
        Ok(())
    }

    #[must_use]
    pub const fn organization_id(&self) -> &OrganizationId {
        &self.organization
    }

    #[must_use]
    pub const fn workspace_id(&self) -> &WorkspaceId {
        &self.workspace
    }

    #[must_use]
    pub const fn project_id(&self) -> &ProjectId {
        &self.project
    }

    #[must_use]
    pub const fn repository_id(&self) -> &RepositoryId {
        &self.repository
    }

    /// Returns the deterministic digest used by sealed Publication authority.
    #[must_use]
    pub fn sha256(&self) -> Sha256Digest {
        sha256_json(self)
    }
}

/// One closed allow/deny value. Deny is always evaluated before allow.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum PolicyPermission {
    Allow,
    Deny,
}

/// Closed request origin retained by the audit adapter.
#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum PublicationPolicyOrigin {
    Local { component: String },
    Network { source_ip: IpAddr },
}

/// One authenticated policy-evaluation request.
///
/// The context is application authority rather than a wire DTO. Its request
/// identity gives every allow or deny decision a stable audit identity.
#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct PublicationPolicyContext {
    requester: PublicationRequester,
    request_id: RequestId,
    scope: RepositoryPolicyScope,
    origin: PublicationPolicyOrigin,
    evidence: PublicationPolicyEvidence,
}

impl PublicationPolicyContext {
    /// Seals the authenticated actor, request, origin, and current policy facts.
    ///
    /// # Errors
    ///
    /// Rejects a malformed actor or request identity.
    pub fn try_new(
        requester: PublicationRequester,
        request_id: RequestId,
        scope: RepositoryPolicyScope,
        origin: PublicationPolicyOrigin,
        evidence: PublicationPolicyEvidence,
    ) -> Result<Self, String> {
        requester.validate()?;
        scope.validate()?;
        if !canonical_prefixed_id(&request_id.0, "req_") {
            return Err("publication policy request identity is invalid".to_owned());
        }
        Ok(Self {
            requester,
            request_id,
            scope,
            origin,
            evidence,
        })
    }

    #[must_use]
    pub const fn requester(&self) -> &PublicationRequester {
        &self.requester
    }

    #[must_use]
    pub const fn request_id(&self) -> &RequestId {
        &self.request_id
    }

    #[must_use]
    pub const fn scope(&self) -> &RepositoryPolicyScope {
        &self.scope
    }

    #[must_use]
    pub const fn origin(&self) -> &PublicationPolicyOrigin {
        &self.origin
    }

    #[must_use]
    pub const fn evidence(&self) -> &PublicationPolicyEvidence {
        &self.evidence
    }
}

/// Closed failure returned by the policy-audit boundary.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct PublicationPolicyAuditError;

impl PublicationPolicyAuditError {
    #[must_use]
    pub const fn unavailable() -> Self {
        Self
    }
}

impl fmt::Display for PublicationPolicyAuditError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("publication policy audit is unavailable")
    }
}

impl std::error::Error for PublicationPolicyAuditError {}

/// Required audit port for every Publication policy evaluation.
///
/// The coordinator does not expose a publish or resume constructor without
/// this port, so a provider adapter cannot skip the recorded decision.
pub trait PublicationPolicyAudit {
    /// Records one exact, secret-safe decision before a durable intent or
    /// provider operation.
    ///
    /// # Errors
    ///
    /// Fails closed when the immutable audit sink cannot retain the decision.
    fn record(
        &mut self,
        decision: &PublicationPolicyDecision,
    ) -> Result<(), PublicationPolicyAuditError>;
}

impl PublicationPolicyOrigin {
    /// Builds a bounded local component origin.
    ///
    /// # Errors
    ///
    /// Rejects unbounded or non-portable component names.
    pub fn local(component: &str) -> Result<Self, String> {
        if !portable_token(component) {
            return Err("publication policy local origin is invalid".to_owned());
        }
        Ok(Self::Local {
            component: component.to_owned(),
        })
    }

    #[must_use]
    pub const fn network(source_ip: IpAddr) -> Self {
        Self::Network { source_ip }
    }
}

/// Trusted current Delivery/verification/Artifact facts used by policy.
///
/// This type is not a wire DTO. Its publication-set digest prevents reuse with
/// another candidate, approval, target, or Artifact.
#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct PublicationPolicyEvidence {
    publication_set_sha256: Sha256Digest,
    repository_scope_sha256: Sha256Digest,
    independent_verification: bool,
    artifact_exportable: bool,
    observed_at_millis: u64,
}

impl PublicationPolicyEvidence {
    /// Seals current verification and Artifact-export facts to one authorization.
    ///
    /// # Errors
    ///
    /// Rejects an invalid authorization or timestamp.
    pub fn try_from_current_facts(
        authorization: &PublicationAuthorization,
        independent_verification: bool,
        artifact_exportable: bool,
        observed_at_millis: u64,
    ) -> Result<Self, String> {
        authorization.validate()?;
        if observed_at_millis == 0 || observed_at_millis > MAX_SAFE_INTEGER {
            return Err("publication policy observation time is invalid".to_owned());
        }
        Ok(Self {
            publication_set_sha256: authorization.publication_set_sha256().clone(),
            repository_scope_sha256: authorization.repository_scope_sha256().clone(),
            independent_verification,
            artifact_exportable,
            observed_at_millis,
        })
    }

    #[must_use]
    pub const fn independent_verification(&self) -> bool {
        self.independent_verification
    }

    #[must_use]
    pub const fn artifact_exportable(&self) -> bool {
        self.artifact_exportable
    }

    #[must_use]
    pub const fn observed_at_millis(&self) -> u64 {
        self.observed_at_millis
    }
}

/// First closed repository policy for Publication.
#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct RepositoryPublicationPolicy {
    scope: RepositoryPolicyScope,
    repository: String,
    allowed_requesters: Vec<PublicationRequester>,
    denied_requesters: Vec<PublicationRequester>,
    allowed_approvers: Vec<UserId>,
    denied_approvers: Vec<UserId>,
    repository_write: PolicyPermission,
    require_independent_verification: bool,
    artifact_export: PolicyPermission,
    max_approval_age_millis: u64,
    digest: Sha256Digest,
}

impl RepositoryPublicationPolicy {
    /// Builds a deterministic repository Publication policy.
    ///
    /// # Errors
    ///
    /// Rejects malformed scope/repository/principal values, empty allow sets,
    /// duplicate members, or an invalid approval lifetime.
    #[allow(clippy::too_many_arguments)]
    pub fn try_new(
        scope: RepositoryPolicyScope,
        repository: impl Into<String>,
        mut allowed_requesters: Vec<PublicationRequester>,
        mut denied_requesters: Vec<PublicationRequester>,
        mut allowed_approvers: Vec<UserId>,
        mut denied_approvers: Vec<UserId>,
        repository_write: PolicyPermission,
        require_independent_verification: bool,
        artifact_export: PolicyPermission,
        max_approval_age_millis: u64,
    ) -> Result<Self, String> {
        scope.validate()?;
        let repository = repository.into();
        if !repository_slug(&repository)
            || allowed_requesters.is_empty()
            || allowed_approvers.is_empty()
            || max_approval_age_millis == 0
            || max_approval_age_millis > MAX_SAFE_INTEGER
        {
            return Err("repository publication policy is incomplete".to_owned());
        }
        for requester in allowed_requesters.iter().chain(&denied_requesters) {
            requester.validate()?;
        }
        for approver in allowed_approvers.iter().chain(&denied_approvers) {
            if !canonical_prefixed_id(&approver.0, "usr_") {
                return Err("repository policy approver identity is invalid".to_owned());
            }
        }
        sort_unique_requesters(&mut allowed_requesters)?;
        sort_unique_requesters(&mut denied_requesters)?;
        sort_unique_users(&mut allowed_approvers)?;
        sort_unique_users(&mut denied_approvers)?;
        let digest = sha256_json(&(
            &scope,
            &repository,
            &allowed_requesters,
            &denied_requesters,
            &allowed_approvers,
            &denied_approvers,
            repository_write,
            require_independent_verification,
            artifact_export,
            max_approval_age_millis,
        ));
        Ok(Self {
            scope,
            repository,
            allowed_requesters,
            denied_requesters,
            allowed_approvers,
            denied_approvers,
            repository_write,
            require_independent_verification,
            artifact_export,
            max_approval_age_millis,
            digest,
        })
    }

    /// Evaluates every trusted fact before a Publication intent or provider call.
    ///
    /// # Errors
    ///
    /// Rejects facts that belong to another repository, authorization, or time.
    #[allow(clippy::too_many_arguments)]
    pub fn evaluate(
        &self,
        context: &PublicationPolicyContext,
        publication_id: &PublicationId,
        authorization: &PublicationAuthorization,
    ) -> Result<PublicationPolicyDecision, String> {
        let requester = context.requester();
        let request_id = context.request_id();
        let origin = context.origin();
        let evidence = context.evidence();
        requester.validate()?;
        authorization.validate()?;
        if !canonical_prefixed_id(&request_id.0, "req_")
            || !canonical_prefixed_id(&publication_id.0, "pub_")
            || context.scope() != &self.scope
            || authorization.target().repository() != self.repository
            || evidence.publication_set_sha256 != *authorization.publication_set_sha256()
            || evidence.repository_scope_sha256 != *authorization.repository_scope_sha256()
            || evidence.repository_scope_sha256 != self.scope.sha256()
            || evidence.observed_at_millis < authorization.approved_at_millis()
        {
            return Err(
                "publication policy facts are stale or belong to another authority".to_owned(),
            );
        }
        let approver = UserId(authorization.approved_by().to_owned());
        let rule = if self.denied_requesters.contains(requester) {
            PublicationPolicyRule::RequesterExplicitDeny
        } else if self.denied_approvers.contains(&approver) {
            PublicationPolicyRule::ApproverExplicitDeny
        } else if self.repository_write == PolicyPermission::Deny {
            PublicationPolicyRule::RepositoryWriteDenied
        } else if self.artifact_export == PolicyPermission::Deny {
            PublicationPolicyRule::ArtifactExportDenied
        } else if !self.allowed_requesters.contains(requester) {
            PublicationPolicyRule::RequesterNotAllowed
        } else if !self.allowed_approvers.contains(&approver) {
            PublicationPolicyRule::ApproverNotAllowed
        } else if self.require_independent_verification && !evidence.independent_verification {
            PublicationPolicyRule::IndependentVerificationRequired
        } else if !evidence.artifact_exportable {
            PublicationPolicyRule::ArtifactNotExportable
        } else if evidence
            .observed_at_millis
            .saturating_sub(authorization.approved_at_millis())
            > self.max_approval_age_millis
        {
            PublicationPolicyRule::ApprovalExpired
        } else {
            PublicationPolicyRule::Allowed
        };
        let effect = if rule == PublicationPolicyRule::Allowed {
            PublicationPolicyEffect::Allow
        } else {
            PublicationPolicyEffect::Deny
        };
        PublicationPolicyDecision::try_new(
            effect,
            rule,
            self.digest.clone(),
            requester.clone(),
            self.scope.clone(),
            request_id.clone(),
            origin.clone(),
            publication_id.clone(),
            authorization.binding().delivery_id().clone(),
            evidence.observed_at_millis,
        )
    }

    #[must_use]
    pub const fn scope(&self) -> &RepositoryPolicyScope {
        &self.scope
    }

    #[must_use]
    pub const fn digest(&self) -> &Sha256Digest {
        &self.digest
    }
}

/// Stable rule that produced one policy decision.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum PublicationPolicyRule {
    RequesterExplicitDeny,
    ApproverExplicitDeny,
    RepositoryWriteDenied,
    ArtifactExportDenied,
    RequesterNotAllowed,
    ApproverNotAllowed,
    IndependentVerificationRequired,
    ArtifactNotExportable,
    ApprovalExpired,
    Allowed,
}

impl PublicationPolicyRule {
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::RequesterExplicitDeny => "publication.requester.denied",
            Self::ApproverExplicitDeny => "publication.approver.denied",
            Self::RepositoryWriteDenied => "publication.repository.write-denied",
            Self::ArtifactExportDenied => "publication.artifact.export-denied",
            Self::RequesterNotAllowed => "publication.requester.not-allowed",
            Self::ApproverNotAllowed => "publication.approver.not-allowed",
            Self::IndependentVerificationRequired => "publication.verification.required",
            Self::ArtifactNotExportable => "publication.artifact.not-exportable",
            Self::ApprovalExpired => "publication.approval.expired",
            Self::Allowed => "publication.allowed",
        }
    }
}

/// Closed policy result.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum PublicationPolicyEffect {
    Allow,
    Deny,
}

/// Secret-safe policy result passed to the Control Plane audit adapter.
#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct PublicationPolicyDecision {
    effect: PublicationPolicyEffect,
    rule: PublicationPolicyRule,
    policy_sha256: Sha256Digest,
    requester: PublicationRequester,
    scope: RepositoryPolicyScope,
    request_id: RequestId,
    origin: PublicationPolicyOrigin,
    publication_id: PublicationId,
    delivery_id: winwincode_domain::DeliveryId,
    occurred_at_millis: u64,
    decision_sha256: Sha256Digest,
}

impl PublicationPolicyDecision {
    #[allow(clippy::too_many_arguments)]
    fn try_new(
        effect: PublicationPolicyEffect,
        rule: PublicationPolicyRule,
        policy_sha256: Sha256Digest,
        requester: PublicationRequester,
        scope: RepositoryPolicyScope,
        request_id: RequestId,
        origin: PublicationPolicyOrigin,
        publication_id: PublicationId,
        delivery_id: winwincode_domain::DeliveryId,
        occurred_at_millis: u64,
    ) -> Result<Self, String> {
        if !canonical_sha256(&policy_sha256)
            || (effect == PublicationPolicyEffect::Allow)
                != (rule == PublicationPolicyRule::Allowed)
        {
            return Err("publication policy decision is invalid".to_owned());
        }
        let decision_sha256 = sha256_json(&(
            effect,
            rule,
            &policy_sha256,
            &requester,
            &scope,
            &request_id,
            &origin,
            &publication_id,
            &delivery_id,
            occurred_at_millis,
        ));
        Ok(Self {
            effect,
            rule,
            policy_sha256,
            requester,
            scope,
            request_id,
            origin,
            publication_id,
            delivery_id,
            occurred_at_millis,
            decision_sha256,
        })
    }

    #[must_use]
    pub const fn effect(&self) -> PublicationPolicyEffect {
        self.effect
    }

    #[must_use]
    pub const fn rule(&self) -> PublicationPolicyRule {
        self.rule
    }

    #[must_use]
    pub const fn policy_sha256(&self) -> &Sha256Digest {
        &self.policy_sha256
    }

    #[must_use]
    pub const fn requester(&self) -> &PublicationRequester {
        &self.requester
    }

    #[must_use]
    pub const fn scope(&self) -> &RepositoryPolicyScope {
        &self.scope
    }

    #[must_use]
    pub const fn request_id(&self) -> &RequestId {
        &self.request_id
    }

    #[must_use]
    pub const fn origin(&self) -> &PublicationPolicyOrigin {
        &self.origin
    }

    #[must_use]
    pub const fn publication_id(&self) -> &PublicationId {
        &self.publication_id
    }

    #[must_use]
    pub const fn delivery_id(&self) -> &winwincode_domain::DeliveryId {
        &self.delivery_id
    }

    #[must_use]
    pub const fn occurred_at_millis(&self) -> u64 {
        self.occurred_at_millis
    }

    #[must_use]
    pub const fn decision_sha256(&self) -> &Sha256Digest {
        &self.decision_sha256
    }
}

fn sort_unique_requesters(values: &mut [PublicationRequester]) -> Result<(), String> {
    values.sort_by_key(PublicationRequester::sort_key);
    if values.windows(2).any(|pair| pair[0] == pair[1]) {
        return Err("repository policy requester list contains duplicates".to_owned());
    }
    Ok(())
}

fn sort_unique_users(values: &mut [UserId]) -> Result<(), String> {
    values.sort_by(|left, right| left.0.cmp(&right.0));
    if values.windows(2).any(|pair| pair[0] == pair[1]) {
        return Err("repository policy approver list contains duplicates".to_owned());
    }
    Ok(())
}

fn portable_token(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 128
        && value.bytes().all(|byte| {
            byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'_' | b':' | b'@' | b'-')
        })
}

fn sha256_json(value: &impl Serialize) -> Sha256Digest {
    let bytes = serde_json::to_vec(value).expect("serializable repository policy value");
    Sha256Digest(format!("sha256:{:x}", Sha256::digest(bytes)))
}
