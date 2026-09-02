// SPDX-License-Identifier: Apache-2.0

//! Deterministic sealed publication fixtures for Rust integration tests.

use winwincode_domain::{
    AttentionItemId, DeliveryId, OrganizationId, ProductSessionId, ProjectId, PublicationId,
    RepositoryId, RequestId, Sha256Digest, UserId, WorkspaceId,
};
use winwincode_storage::ProductStateStorage;
use winwincode_storage::{ReceiptActorKey, ReceiptIdentity, ReceiptScopeKey};

use crate::PublicationEnterpriseAttribution;
use crate::coordinator::{
    Publication, PublicationCancelCommand, PublicationCommandContext, PublicationCoordinator,
    PublicationError, PublicationLedger, PublicationPublishCommand,
};
use crate::facts::{
    PublicationAuthorization, PublicationFactBinding, PublicationSourceIssue, PublicationTarget,
    raw_sha256_json,
};
use crate::operation::PublicationOperation;
use crate::operation::PublicationPort;
use crate::policy::{
    PolicyPermission, PublicationPolicyAudit, PublicationPolicyAuditError,
    PublicationPolicyContext, PublicationPolicyDecision, PublicationPolicyEvidence,
    PublicationPolicyOrigin, PublicationRequester, RepositoryPolicyScope,
    RepositoryPublicationPolicy,
};

pub struct CurrentPublicationFixture {
    authorization: PublicationAuthorization,
    attribution: PublicationEnterpriseAttribution,
    publish_command: PublicationPublishCommand,
    publish_context: PublicationCommandContext,
    resume_time_millis: u64,
}

impl CurrentPublicationFixture {
    #[must_use]
    pub const fn authorization(&self) -> &PublicationAuthorization {
        &self.authorization
    }

    #[must_use]
    pub const fn attribution(&self) -> &PublicationEnterpriseAttribution {
        &self.attribution
    }

    #[must_use]
    pub const fn publish_command(&self) -> &PublicationPublishCommand {
        &self.publish_command
    }

    #[must_use]
    pub const fn publish_context(&self) -> &PublicationCommandContext {
        &self.publish_context
    }

    #[must_use]
    pub const fn publication_id(&self) -> &PublicationId {
        self.publish_command.publication_id()
    }

    #[must_use]
    pub const fn resume_time_millis(&self) -> u64 {
        self.resume_time_millis
    }
}

#[must_use]
/// Builds one deterministic, fully sealed current-publication fixture.
///
/// # Panics
///
/// Panics only if a repository-owned canonical fixture literal becomes invalid.
pub fn current_publication_fixture() -> CurrentPublicationFixture {
    let target = PublicationTarget::try_github(
        "example/widget",
        "main",
        "example/widget",
        "winwincode/delivery",
    )
    .expect("canonical target");
    let source =
        PublicationSourceIssue::try_github("example/widget", 7).expect("canonical source issue");
    let binding = PublicationFactBinding::try_new(
        DeliveryId("dlv_00000000000000000000000001".to_owned()),
        21,
        "spec_00000000000000000000000001",
        1,
        format!("git-candidate:sha256:{}", "a".repeat(64)),
        "c".repeat(64),
        "verdict:fixture:pass",
        AttentionItemId("att_00000000000000000000000001".to_owned()),
        "d".repeat(64),
        raw_sha256_json(&target),
    )
    .expect("current publication binding");
    let authorization = PublicationAuthorization::try_from_current_facts(
        binding,
        source,
        target.clone(),
        "a".repeat(40),
        "art_00000000000000000000000001",
        Sha256Digest(format!("sha256:{}", "b".repeat(64))),
        "usr_00000000000000000000000001",
        1_000,
        current_policy_scope().sha256(),
    )
    .expect("sealed current publication authorization");
    let publish_command = PublicationPublishCommand::try_new(
        PublicationId("pub_00000000000000000000000001".to_owned()),
        authorization.binding.delivery_id().clone(),
        authorization.candidate_digest.clone(),
        target,
    )
    .expect("canonical publish command");
    let receipt_identity = ReceiptIdentity::new(
        ReceiptActorKey::from_encoded(b"fixture-publication-actor".to_vec()).expect("actor key"),
        ReceiptScopeKey::from_encoded(b"fixture-publication-repository-scope".to_vec())
            .expect("scope key"),
        RequestId("req_00000000000000000000000001".to_owned()),
    )
    .expect("receipt identity");
    let publish_context = PublicationCommandContext::try_new(
        receipt_identity,
        Sha256Digest(format!("sha256:{}", "e".repeat(64))),
        0,
        1_100,
    )
    .expect("publish command context");
    let attribution = PublicationEnterpriseAttribution::try_new(
        &current_policy_scope(),
        authorization.binding().delivery_id().clone(),
        ProductSessionId("psn_00000000000000000000000001".to_owned()),
        UserId("usr_00000000000000000000000002".to_owned()),
    )
    .expect("current publication enterprise attribution");
    CurrentPublicationFixture {
        authorization,
        attribution,
        publish_command,
        publish_context,
        resume_time_millis: 2_000,
    }
}

/// Returns the exact ordered provider operations for the current publication fixture.
#[must_use]
pub fn current_publication_operations() -> Vec<PublicationOperation> {
    let fixture = current_publication_fixture();
    PublicationOperation::ordered(fixture.authorization())
}

struct AcceptingPolicyAudit;

impl PublicationPolicyAudit for AcceptingPolicyAudit {
    fn record(
        &mut self,
        _decision: &PublicationPolicyDecision,
    ) -> Result<(), PublicationPolicyAuditError> {
        Ok(())
    }
}

/// Policy-guarded coordinator used only by direct Publication integration tests.
pub struct CurrentPublicationCoordinator<'storage, 'port> {
    coordinator: PublicationCoordinator<'storage, 'port, 'static>,
}

impl CurrentPublicationCoordinator<'_, '_> {
    /// Applies the fixed repository policy before persisting the fixture intent.
    ///
    /// # Errors
    ///
    /// Returns the production coordinator error unchanged.
    pub fn publish(
        &mut self,
        context: &PublicationCommandContext,
        command: &PublicationPublishCommand,
        authorization: &PublicationAuthorization,
    ) -> Result<Publication, PublicationError> {
        let fixture = current_publication_fixture();
        self.publish_with_attribution(context, command, authorization, fixture.attribution())
    }

    /// Applies the fixed policy while allowing tests to prove an attribution mismatch fails closed.
    ///
    /// # Errors
    ///
    /// Returns the production coordinator error unchanged.
    pub fn publish_with_attribution(
        &mut self,
        context: &PublicationCommandContext,
        command: &PublicationPublishCommand,
        authorization: &PublicationAuthorization,
        attribution: &PublicationEnterpriseAttribution,
    ) -> Result<Publication, PublicationError> {
        let policy = current_repository_policy(authorization);
        let policy_context = current_policy_context(
            authorization,
            context.receipt_identity().request_id().clone(),
            context.occurred_at_millis(),
        );
        self.coordinator.publish(
            context,
            command,
            authorization,
            attribution,
            &policy_context,
            &policy,
        )
    }

    /// Applies the fixed repository policy before the next provider operation.
    ///
    /// # Errors
    ///
    /// Returns the production coordinator error unchanged.
    pub fn resume(
        &mut self,
        publication_id: &PublicationId,
        occurred_at_millis: u64,
    ) -> Result<Publication, PublicationError> {
        let fixture = current_publication_fixture();
        let policy = current_repository_policy(fixture.authorization());
        let policy_context = current_policy_context(
            fixture.authorization(),
            RequestId(format!("req_{occurred_at_millis:026}")),
            occurred_at_millis,
        );
        self.coordinator
            .resume(publication_id, occurred_at_millis, &policy_context, &policy)
    }

    /// Reads the exact durable Publication fixture.
    ///
    /// # Errors
    ///
    /// Returns the production coordinator error unchanged.
    pub fn get(&self, publication_id: &PublicationId) -> Result<Publication, PublicationError> {
        self.coordinator.get(publication_id)
    }

    /// Cancels the exact durable Publication fixture.
    ///
    /// # Errors
    ///
    /// Returns the production coordinator error unchanged.
    pub fn cancel(
        &mut self,
        context: &PublicationCommandContext,
        command: &PublicationCancelCommand,
    ) -> Result<Publication, PublicationError> {
        self.coordinator.cancel(context, command)
    }
}

/// Constructs the only direct-test coordinator. Production callers use the
/// Control Plane application seam and its immutable `AuditStore` adapter.
pub fn current_policy_coordinator<'storage, 'port>(
    storage: &'storage mut dyn ProductStateStorage,
    port: &'port mut dyn PublicationPort,
) -> CurrentPublicationCoordinator<'storage, 'port> {
    CurrentPublicationCoordinator {
        coordinator: PublicationCoordinator::new(
            PublicationLedger::new(storage),
            port,
            Box::new(AcceptingPolicyAudit),
        ),
    }
}

fn current_repository_policy(
    authorization: &PublicationAuthorization,
) -> RepositoryPublicationPolicy {
    RepositoryPublicationPolicy::try_new(
        current_policy_scope(),
        authorization.target().repository(),
        vec![PublicationRequester::User(UserId(
            "usr_00000000000000000000000002".to_owned(),
        ))],
        Vec::new(),
        vec![UserId(authorization.approved_by().to_owned())],
        Vec::new(),
        PolicyPermission::Allow,
        true,
        PolicyPermission::Allow,
        10_000,
    )
    .expect("closed fixture repository policy")
}

fn current_policy_context(
    authorization: &PublicationAuthorization,
    request_id: RequestId,
    observed_at_millis: u64,
) -> PublicationPolicyContext {
    PublicationPolicyContext::try_new(
        PublicationRequester::User(UserId("usr_00000000000000000000000002".to_owned())),
        request_id,
        current_policy_scope(),
        PublicationPolicyOrigin::local("publication-test").expect("fixture local origin"),
        PublicationPolicyEvidence::try_from_current_facts(
            authorization,
            true,
            true,
            observed_at_millis,
        )
        .expect("sealed fixture policy evidence"),
    )
    .expect("sealed fixture policy context")
}

fn current_policy_scope() -> RepositoryPolicyScope {
    RepositoryPolicyScope::try_new(
        OrganizationId("org_00000000000000000000000001".to_owned()),
        WorkspaceId("wsp_00000000000000000000000001".to_owned()),
        ProjectId("prj_00000000000000000000000001".to_owned()),
        RepositoryId("rep_00000000000000000000000001".to_owned()),
    )
    .expect("canonical fixture policy scope")
}

/// Builds exact provider operations for an explicitly configured GitHub test repository.
///
/// The remaining Delivery, candidate, Artifact, verdict, approval, and scope facts are fixed
/// repository-owned test identities. This helper is feature-gated and cannot construct production
/// publication authority.
///
/// # Errors
///
/// Rejects an invalid target, source issue, commit identity, or derived sealed authorization.
pub fn github_publication_operations_fixture(
    target: PublicationTarget,
    source: PublicationSourceIssue,
    commit_id: impl Into<String>,
) -> Result<Vec<PublicationOperation>, String> {
    let binding = PublicationFactBinding::try_new(
        DeliveryId("dlv_00000000000000000000000001".to_owned()),
        21,
        "spec_00000000000000000000000001",
        1,
        format!("git-candidate:sha256:{}", "a".repeat(64)),
        "c".repeat(64),
        "verdict:fixture:pass",
        AttentionItemId("att_00000000000000000000000001".to_owned()),
        "d".repeat(64),
        raw_sha256_json(&target),
    )?;
    let authorization = PublicationAuthorization::try_from_current_facts(
        binding,
        source,
        target,
        commit_id,
        "art_00000000000000000000000001",
        Sha256Digest(format!("sha256:{}", "b".repeat(64))),
        "usr_00000000000000000000000001",
        1_000,
        current_policy_scope().sha256(),
    )?;
    Ok(PublicationOperation::ordered(&authorization))
}
