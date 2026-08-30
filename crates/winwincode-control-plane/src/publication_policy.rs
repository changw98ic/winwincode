// SPDX-License-Identifier: Apache-2.0

use std::{fmt, path::Path};

use sha2::{Digest, Sha256};
use winwincode_api::generated::{
    Actor, CommandEnvelope, CommandName, ErrorCode, ErrorDetailValue,
    PublicationPublishCommand as ApiPublicationPublishCommand, PublicationTargetProvider, Scope,
};
use winwincode_audit::{
    AuditAccess, AuditAction, AuditActor, AuditError, AuditEvent, AuditEventId, AuditOrigin,
    AuditOutcome, AuditPage, AuditRetention, AuditScope, AuditState, AuditSubject,
};
use winwincode_domain::{RequestId, SchemaVersion};
use winwincode_publication::{
    Publication, PublicationAuthorization, PublicationCommandContext, PublicationCoordinator,
    PublicationEnterpriseAttribution, PublicationError, PublicationErrorKind, PublicationLedger,
    PublicationMeteringLedger, PublicationPolicyAudit, PublicationPolicyAuditError,
    PublicationPolicyContext, PublicationPolicyDecision, PublicationPolicyEffect,
    PublicationPolicyEvidence, PublicationPolicyOrigin, PublicationPort, PublicationPublishCommand,
    PublicationReadLedger, PublicationRequester, PublicationState, PublicationTarget,
    RepositoryPolicyScope, RepositoryPublicationPolicy,
};
use winwincode_storage::ProductStateStorage;

use crate::{
    ControlPlane, DurableEnterpriseQuotaAdmission, PublicationAuthorityError,
    PublicationAuthorityErrorKind, PublicationEnterpriseQuotaSaga,
    PublicationEnterpriseUsageReconciler, PublicationProviderRegistryError,
    PublicationProviderRegistryErrorKind, StorageError, command_receipt,
    publication_enterprise_quota::publication_quota_requested_at,
};

/// Failure at the single generated-command → policy → audit → Publication seam.
#[derive(Debug)]
pub enum PublicationCommandError {
    InvalidInput(String),
    ReadCursorExpired,
    Authority(PublicationAuthorityError),
    Provider(PublicationProviderRegistryError),
    EnterprisePolicyDenied,
    EnterprisePolicyUnavailable,
    PolicyDenied(Box<PublicationPolicyDecision>),
    AuditUnavailable(AuditError),
    Publication(PublicationError),
}

impl PublicationCommandError {
    #[must_use]
    pub const fn public_code(&self) -> ErrorCode {
        match self {
            Self::InvalidInput(_) => ErrorCode::InvalidRequest,
            Self::ReadCursorExpired => ErrorCode::ReadCursorExpired,
            Self::Authority(error) => match error.kind() {
                PublicationAuthorityErrorKind::InvalidConfiguration => {
                    ErrorCode::ServiceUnavailable
                }
                PublicationAuthorityErrorKind::TrustedFactsUnavailable => {
                    ErrorCode::TrustedFactsUnavailable
                }
            },
            Self::Provider(error) => match error.kind() {
                PublicationProviderRegistryErrorKind::PermissionDenied => {
                    ErrorCode::PermissionDenied
                }
                PublicationProviderRegistryErrorKind::NotConfigured
                | PublicationProviderRegistryErrorKind::Unavailable => {
                    ErrorCode::ServiceUnavailable
                }
            },
            Self::EnterprisePolicyDenied | Self::PolicyDenied(_) => ErrorCode::PermissionDenied,
            Self::EnterprisePolicyUnavailable | Self::AuditUnavailable(_) => {
                ErrorCode::ServiceUnavailable
            }
            Self::Publication(error) => match error.kind() {
                PublicationErrorKind::InvalidInput => ErrorCode::InvalidRequest,
                PublicationErrorKind::StaleAuthority => ErrorCode::TrustedFactsUnavailable,
                PublicationErrorKind::PolicyDenied => ErrorCode::PermissionDenied,
                PublicationErrorKind::RequestConflict => ErrorCode::IdempotencyConflict,
                PublicationErrorKind::RevisionConflict => ErrorCode::RevisionConflict,
                PublicationErrorKind::AlreadyExists | PublicationErrorKind::WrongState => {
                    ErrorCode::WrongState
                }
                PublicationErrorKind::NotFound => ErrorCode::ResourceNotFound,
                PublicationErrorKind::AuditUnavailable
                | PublicationErrorKind::PortContract
                | PublicationErrorKind::Storage
                | PublicationErrorKind::Corrupt => ErrorCode::ServiceUnavailable,
            },
        }
    }

    #[must_use]
    pub fn public_details(&self) -> winwincode_api::generated::ErrorDetails {
        let mut details = winwincode_api::generated::ErrorDetails::new();
        if let Self::PolicyDenied(decision) = self {
            details.insert(
                "ruleId".to_owned(),
                ErrorDetailValue::Variant4(decision.rule().as_str().to_owned()),
            );
            details.insert(
                "repositoryId".to_owned(),
                ErrorDetailValue::Variant4(decision.scope().repository_id().0.clone()),
            );
            details.insert(
                "publicationId".to_owned(),
                ErrorDetailValue::Variant4(decision.publication_id().0.clone()),
            );
        }
        details
    }

    #[must_use]
    pub const fn retryable(&self) -> bool {
        matches!(self, Self::AuditUnavailable(_))
            || matches!(self, Self::EnterprisePolicyUnavailable)
            || matches!(
                self,
                Self::Provider(error)
                    if matches!(
                        error.kind(),
                        PublicationProviderRegistryErrorKind::Unavailable
                    )
            )
            || matches!(
                self,
                Self::Publication(error)
                    if matches!(
                        error.kind(),
                        PublicationErrorKind::PortContract
                            | PublicationErrorKind::Storage
                            | PublicationErrorKind::Corrupt
                    )
            )
    }

    #[must_use]
    pub fn decision(&self) -> Option<&PublicationPolicyDecision> {
        match self {
            Self::PolicyDenied(decision) => Some(decision.as_ref()),
            Self::InvalidInput(_)
            | Self::ReadCursorExpired
            | Self::Authority(_)
            | Self::Provider(_)
            | Self::EnterprisePolicyDenied
            | Self::EnterprisePolicyUnavailable
            | Self::AuditUnavailable(_)
            | Self::Publication(_) => None,
        }
    }
}

impl fmt::Display for PublicationCommandError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidInput(message) => formatter.write_str(message),
            Self::ReadCursorExpired => formatter.write_str("publication list cursor expired"),
            Self::Authority(error) => write!(formatter, "publication authority failed: {error}"),
            Self::Provider(error) => write!(formatter, "publication provider failed: {error}"),
            Self::EnterprisePolicyDenied => {
                formatter.write_str("enterprise Publication Policy denied the request")
            }
            Self::EnterprisePolicyUnavailable => {
                formatter.write_str("enterprise Publication Policy is unavailable")
            }
            Self::PolicyDenied(decision) => {
                write!(
                    formatter,
                    "publication denied by {}",
                    decision.rule().as_str()
                )
            }
            Self::AuditUnavailable(_) => {
                formatter.write_str("publication policy audit is unavailable")
            }
            Self::Publication(error) => write!(formatter, "publication failed: {error}"),
        }
    }
}

impl std::error::Error for PublicationCommandError {}

impl From<PublicationError> for PublicationCommandError {
    fn from(error: PublicationError) -> Self {
        match error.kind() {
            PublicationErrorKind::PolicyDenied => error.policy_decision().cloned().map_or_else(
                || Self::Publication(error),
                |decision| Self::PolicyDenied(Box::new(decision)),
            ),
            PublicationErrorKind::AuditUnavailable => {
                Self::AuditUnavailable(AuditError::unavailable())
            }
            _ => Self::Publication(error),
        }
    }
}

impl From<AuditError> for PublicationCommandError {
    fn from(error: AuditError) -> Self {
        Self::AuditUnavailable(error)
    }
}

impl From<PublicationAuthorityError> for PublicationCommandError {
    fn from(error: PublicationAuthorityError) -> Self {
        Self::Authority(error)
    }
}

impl From<PublicationProviderRegistryError> for PublicationCommandError {
    fn from(error: PublicationProviderRegistryError) -> Self {
        Self::Provider(error)
    }
}

impl ControlPlane {
    /// Applies the repository policy and durably audits its exact rule before
    /// persisting one Publication intent. Provider effects remain deferred to
    /// the policy-guarded resume seam.
    ///
    /// # Errors
    ///
    /// Returns `PERMISSION_DENIED` facts for a recorded policy denial and fails
    /// closed before intent persistence when policy facts or audit storage are unavailable.
    #[allow(clippy::too_many_arguments)]
    pub fn commit_publication_publish(
        &mut self,
        command: &ApiPublicationPublishCommand,
        authorization: &PublicationAuthorization,
        attribution: &PublicationEnterpriseAttribution,
        policy: &RepositoryPublicationPolicy,
        evidence: &PublicationPolicyEvidence,
        origin: &PublicationPolicyOrigin,
        port: &mut dyn PublicationPort,
    ) -> Result<Publication, PublicationCommandError> {
        let mapped = mapped_publish(command, evidence.observed_at_millis())?;
        let policy_context = PublicationPolicyContext::try_new(
            requester(&command.actor),
            command.request_id.clone(),
            repository_policy_scope(command)?,
            origin.clone(),
            evidence.clone(),
        )
        .map_err(PublicationCommandError::InvalidInput)?;

        let storage = match &mut self.storage {
            Some(storage) => storage.as_mut(),
            None => {
                return Err(PublicationCommandError::Publication(
                    PublicationError::from(StorageError::adapter(
                        "Control Plane storage is closed",
                    )),
                ));
            }
        };
        let audit: Box<dyn PublicationPolicyAudit + '_> = self.audit_store.as_mut().map_or_else(
            || Box::new(UnavailablePolicyAudit) as Box<dyn PublicationPolicyAudit>,
            |store| Box::new(ControlPlanePolicyAudit { store }),
        );
        let publication = PublicationCoordinator::new(PublicationLedger::new(storage), port, audit)
            .publish(
                &mapped.context,
                &mapped.command,
                authorization,
                attribution,
                &policy_context,
                policy,
            )
            .map_err(PublicationCommandError::from)?;
        self.record_publication_result(&publication, &policy_context)?;
        Ok(publication)
    }

    /// Reconciles one durable Publication only after the current repository
    /// policy decision has been recorded. This is the application seam used by
    /// recovery workers; the provider adapter has no direct coordinator path.
    ///
    /// # Errors
    ///
    /// Fails closed before a provider lookup or write when current policy facts
    /// are denied, stale, or cannot be written to the immutable audit store.
    pub fn resume_publication(
        &mut self,
        publication_id: &winwincode_domain::PublicationId,
        policy_context: &PublicationPolicyContext,
        policy: &RepositoryPublicationPolicy,
        port: &mut dyn PublicationPort,
    ) -> Result<Publication, PublicationCommandError> {
        let attribution =
            PublicationMeteringLedger::new(self.storage_ref().map_err(publication_storage_error)?)
                .attribution(publication_id)
                .map_err(|_| {
                    publication_quota_error("Publication enterprise attribution is unavailable")
                })?;
        let quota_directory = self
            .local_database_path()
            .and_then(Path::parent)
            .map(Path::to_path_buf)
            .ok_or_else(|| {
                publication_quota_error("local Publication quota database is unavailable")
            })?;
        let quota_storage = winwincode_storage::SqliteStorage::open(&quota_directory)
            .map_err(publication_storage_error)?;
        let mut quota = DurableEnterpriseQuotaAdmission::new(quota_storage);
        let requested_at =
            PublicationReadLedger::new(self.storage_ref().map_err(publication_storage_error)?)
                .get(publication_id)
                .map_err(PublicationCommandError::from)
                .and_then(|publication| {
                    publication_quota_requested_at(publication.approved_at_millis())
                        .map_err(publication_storage_error)
                })?;
        let publication = {
            let mut guarded = PublicationEnterpriseQuotaSaga::new(
                &mut quota,
                port,
                &attribution,
                publication_id,
                requested_at,
            );
            let storage = self
                .storage
                .as_mut()
                .ok_or_else(|| publication_quota_error("Control Plane storage is closed"))?;
            let audit: Box<dyn PublicationPolicyAudit + '_> =
                self.audit_store.as_mut().map_or_else(
                    || Box::new(UnavailablePolicyAudit) as Box<dyn PublicationPolicyAudit>,
                    |store| Box::new(ControlPlanePolicyAudit { store }),
                );
            PublicationCoordinator::new(
                PublicationLedger::new(storage.as_mut()),
                &mut guarded,
                audit,
            )
            .resume(
                publication_id,
                policy_context.evidence().observed_at_millis(),
                policy_context,
                policy,
            )
            .map_err(PublicationCommandError::from)?
        };
        quota.close().map_err(publication_storage_error)?;
        let mut usage_storage = winwincode_storage::SqliteStorage::open(&quota_directory)
            .map_err(publication_storage_error)?;
        let reconciliation = PublicationEnterpriseUsageReconciler::new(&mut usage_storage)
            .reconcile_exact_publication(publication_id)
            .map_err(|_| {
                publication_quota_error("Publication enterprise Usage reconciliation failed")
            });
        let close = Box::new(usage_storage).close();
        reconciliation?;
        close.map_err(publication_storage_error)?;
        if matches!(
            publication.state(),
            PublicationState::Published | PublicationState::Cancelled | PublicationState::Failed
        ) {
            self.finalize_candidate_git_for_terminal_delivery(publication.binding().delivery_id())
                .map_err(|error| {
                    PublicationCommandError::Publication(PublicationError::from(
                        StorageError::adapter(error.to_string()),
                    ))
                })?;
        }
        self.record_publication_result(&publication, policy_context)?;
        Ok(publication)
    }

    /// Reads retained audit facts through an already-authorized exact scope.
    /// This is the Rust application seam; no HTTP audit query is introduced here.
    ///
    /// # Errors
    ///
    /// Fails when the audit store is unavailable, corrupt, or the page is invalid.
    pub fn read_audit(
        &self,
        access: &AuditAccess,
        after_sequence: u64,
        limit: usize,
        as_of_millis: u64,
    ) -> Result<AuditPage, AuditError> {
        self.audit_store
            .as_ref()
            .ok_or_else(AuditError::unavailable)?
            .read(access, after_sequence, limit, as_of_millis)
    }

    fn record_publication_result(
        &mut self,
        publication: &Publication,
        context: &PublicationPolicyContext,
    ) -> Result<(), PublicationCommandError> {
        let Some(store) = self.audit_store.as_mut() else {
            // A new intent or provider operation cannot reach this point without
            // the policy audit. The only audit-less success is an exact durable
            // command-receipt replay whose original result was already audited.
            return Ok(());
        };
        append_publication_result_audit(store, publication, context)
            .map_err(PublicationCommandError::AuditUnavailable)
    }
}

fn publication_storage_error(error: StorageError) -> PublicationCommandError {
    PublicationCommandError::Publication(PublicationError::from(error))
}

fn publication_quota_error(message: &'static str) -> PublicationCommandError {
    publication_storage_error(StorageError::adapter(message))
}

struct MappedPublish {
    context: PublicationCommandContext,
    command: PublicationPublishCommand,
}

fn mapped_publish(
    command: &ApiPublicationPublishCommand,
    occurred_at_millis: u64,
) -> Result<MappedPublish, PublicationCommandError> {
    if command.schema_version != SchemaVersion::WinwincodeV1
        || command.expected_revision.0 != 0
        || command.payload.target.provider != PublicationTargetProvider::Github
    {
        return Err(PublicationCommandError::InvalidInput(
            "publication.publish command is invalid".to_owned(),
        ));
    }
    let target = PublicationTarget::try_github(
        command.payload.target.repository.0.clone(),
        command.payload.target.base_branch.clone(),
        command.payload.target.head_repository.0.clone(),
        command.payload.target.head_branch.clone(),
    )
    .map_err(PublicationCommandError::InvalidInput)?;
    let domain_command = PublicationPublishCommand::try_new(
        command.payload.publication_id.clone(),
        command.payload.delivery_id.clone(),
        command.payload.candidate_digest.clone(),
        target,
    )?;
    let generic = CommandEnvelope {
        actor: command.actor.clone(),
        command: CommandName::PublicationPublish,
        expected_revision: command.expected_revision.clone(),
        payload: serde_json::to_value(&command.payload).map_err(|_| {
            PublicationCommandError::InvalidInput(
                "publication.publish payload cannot be encoded".to_owned(),
            )
        })?,
        request_id: command.request_id.clone(),
        schema_version: command.schema_version.clone(),
        scope: Scope::RepositoryScope(command.scope.clone()),
    };
    let (receipt_identity, command_digest) = command_receipt(&generic)
        .map_err(|error| PublicationCommandError::InvalidInput(error.to_string()))?;
    let context = PublicationCommandContext::try_new(
        receipt_identity,
        command_digest,
        0,
        occurred_at_millis,
    )?;
    Ok(MappedPublish {
        context,
        command: domain_command,
    })
}

fn requester(actor: &Actor) -> PublicationRequester {
    match actor {
        Actor::UserActor(actor) => PublicationRequester::User(actor.id.clone()),
        Actor::ServiceAccountActor(actor) => PublicationRequester::ServiceAccount(actor.id.clone()),
        Actor::SystemActor(actor) => PublicationRequester::System(actor.id.clone()),
    }
}

fn repository_policy_scope(
    command: &ApiPublicationPublishCommand,
) -> Result<RepositoryPolicyScope, PublicationCommandError> {
    RepositoryPolicyScope::try_new(
        command.scope.organization_id.clone(),
        command.scope.workspace_id.clone(),
        command.scope.project_id.clone(),
        command.scope.repository_id.clone(),
    )
    .map_err(PublicationCommandError::InvalidInput)
}

struct ControlPlanePolicyAudit<'store> {
    store: &'store mut winwincode_audit::AuditStore,
}

struct UnavailablePolicyAudit;

impl PublicationPolicyAudit for UnavailablePolicyAudit {
    fn record(
        &mut self,
        _decision: &PublicationPolicyDecision,
    ) -> Result<(), PublicationPolicyAuditError> {
        Err(PublicationPolicyAuditError::unavailable())
    }
}

impl PublicationPolicyAudit for ControlPlanePolicyAudit<'_> {
    fn record(
        &mut self,
        decision: &PublicationPolicyDecision,
    ) -> Result<(), PublicationPolicyAuditError> {
        append_policy_audit(self.store, decision)
            .map_err(|_| PublicationPolicyAuditError::unavailable())
    }
}

fn append_policy_audit(
    store: &mut winwincode_audit::AuditStore,
    decision: &PublicationPolicyDecision,
) -> Result<(), AuditError> {
    let event_id = AuditEventId::from_digest(decision.decision_sha256())?;
    let actor = match decision.requester() {
        PublicationRequester::User(id) => AuditActor::User(id.clone()),
        PublicationRequester::ServiceAccount(id) => AuditActor::ServiceAccount(id.clone()),
        PublicationRequester::System(id) => AuditActor::System(id.clone()),
    };
    let scope = AuditScope::repository(
        decision.scope().organization_id().clone(),
        decision.scope().workspace_id().clone(),
        decision.scope().project_id().clone(),
        decision.scope().repository_id().clone(),
    )?;
    let origin = match decision.origin() {
        PublicationPolicyOrigin::Local { component } => AuditOrigin::local(component),
        PublicationPolicyOrigin::Network { source_ip } => Ok(AuditOrigin::network(*source_ip)),
    }?;
    let action = AuditAction::policy(decision.rule().as_str())?;
    let state = AuditState::unchanged(Some(decision.policy_sha256().clone()))?;
    let subject = AuditSubject::new()
        .with_delivery(decision.delivery_id().clone())
        .with_publication(decision.publication_id().clone());
    let event = match decision.effect() {
        PublicationPolicyEffect::Allow => AuditEvent::succeeded(
            event_id,
            decision.occurred_at_millis(),
            actor,
            scope,
            decision.request_id().clone(),
            action,
            state,
            origin,
            subject,
            "policy.allowed",
            AuditRetention::Indefinite,
        ),
        PublicationPolicyEffect::Deny => AuditEvent::rejected(
            event_id,
            decision.occurred_at_millis(),
            actor,
            scope,
            decision.request_id().clone(),
            action,
            state,
            origin,
            subject,
            "policy.denied",
            AuditRetention::Indefinite,
        ),
    }?;
    store.append(&event).map(|_| ())
}

fn append_publication_result_audit(
    store: &mut winwincode_audit::AuditStore,
    publication: &Publication,
    context: &PublicationPolicyContext,
) -> Result<(), AuditError> {
    let publication_bytes =
        serde_json::to_vec(publication).map_err(|_| AuditError::unavailable())?;
    let publication_digest = sha256_digest(&publication_bytes);
    let event_identity_digest = sha256_digest(
        [
            b"winwincode.publication-result-audit.v1".as_slice(),
            &[0],
            publication_digest.0.as_bytes(),
            &[0],
            context.request_id().0.as_bytes(),
        ]
        .concat()
        .as_slice(),
    );
    let event_id = AuditEventId::from_digest(&event_identity_digest)?;
    let actor = match context.requester() {
        PublicationRequester::User(id) => AuditActor::User(id.clone()),
        PublicationRequester::ServiceAccount(id) => AuditActor::ServiceAccount(id.clone()),
        PublicationRequester::System(id) => AuditActor::System(id.clone()),
    };
    let scope = AuditScope::repository(
        context.scope().organization_id().clone(),
        context.scope().workspace_id().clone(),
        context.scope().project_id().clone(),
        context.scope().repository_id().clone(),
    )?;
    let origin = match context.origin() {
        PublicationPolicyOrigin::Local { component } => AuditOrigin::local(component),
        PublicationPolicyOrigin::Network { source_ip } => Ok(AuditOrigin::network(*source_ip)),
    }?;
    let action = AuditAction::publication("publication.state")?;
    let state = AuditState::unchanged(Some(publication_digest))?;
    let subject = AuditSubject::new()
        .with_delivery(publication.binding().delivery_id().clone())
        .with_publication(publication.id().clone());
    let (outcome, result_code) = match publication.state() {
        PublicationState::Pending => (AuditOutcome::Succeeded, "publication.intent-recorded"),
        PublicationState::Publishing => (AuditOutcome::Failed, "publication.incomplete"),
        PublicationState::Published => (AuditOutcome::Succeeded, "publication.published"),
        PublicationState::Failed => (AuditOutcome::Failed, "publication.failed"),
        PublicationState::Cancelled => (AuditOutcome::Rejected, "publication.cancelled"),
    };
    let event = PublicationResultAuditEvent {
        event_id,
        occurred_at_millis: publication.updated_at_millis(),
        actor,
        scope,
        request_id: context.request_id().clone(),
        action,
        state,
        origin,
        subject,
        result_code,
    }
    .finish(outcome)?;
    store.append(&event).map(|_| ())
}

struct PublicationResultAuditEvent<'result> {
    event_id: AuditEventId,
    occurred_at_millis: u64,
    actor: AuditActor,
    scope: AuditScope,
    request_id: RequestId,
    action: AuditAction,
    state: AuditState,
    origin: AuditOrigin,
    subject: AuditSubject,
    result_code: &'result str,
}

impl PublicationResultAuditEvent<'_> {
    fn finish(self, outcome: AuditOutcome) -> Result<AuditEvent, AuditError> {
        let Self {
            event_id,
            occurred_at_millis,
            actor,
            scope,
            request_id,
            action,
            state,
            origin,
            subject,
            result_code,
        } = self;
        let retention = AuditRetention::Indefinite;
        match outcome {
            AuditOutcome::Succeeded => AuditEvent::succeeded(
                event_id,
                occurred_at_millis,
                actor,
                scope,
                request_id,
                action,
                state,
                origin,
                subject,
                result_code,
                retention,
            ),
            AuditOutcome::Rejected => AuditEvent::rejected(
                event_id,
                occurred_at_millis,
                actor,
                scope,
                request_id,
                action,
                state,
                origin,
                subject,
                result_code,
                retention,
            ),
            AuditOutcome::Failed => AuditEvent::failed(
                event_id,
                occurred_at_millis,
                actor,
                scope,
                request_id,
                action,
                state,
                origin,
                subject,
                result_code,
                retention,
            ),
        }
    }
}

fn sha256_digest(bytes: &[u8]) -> winwincode_domain::Sha256Digest {
    winwincode_domain::Sha256Digest(format!("sha256:{:x}", Sha256::digest(bytes)))
}
