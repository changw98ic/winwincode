// SPDX-License-Identifier: Apache-2.0

//! Receipt-first atomic transaction for canonical non-dispatch Delivery commands.

use sha2::{Digest, Sha256};
use winwincode_api::generated::{
    Actor, CommandEnvelope, CommandName, ControlPlaneWebSocketDeliveryChangedEvent,
    DeliveryAdvancePayload, DeliveryCreatePayload, DeliveryResolveAttentionPayload,
    DeliverySpecInput, DeliveryUpdateSpecPayload, RepositoryScope, Scope,
};
use winwincode_delivery::{
    application::{
        CoordinationError, CoordinationErrorCode,
        attention::ResolvedAttentionTransition,
        stage::{StageAdvanceEffect, StageAdvanceResult},
        task::validate_create_tasks_empty,
    },
    domain::{
        AcceptanceCriterion, AcceptanceCriterionId, AttentionItemStatus, DELIVERY_SCHEMA_VERSION,
        Delivery, DeliveryPublicationTarget, DeliverySnapshot, DeliverySourceRef, DeliverySpec,
        DeliverySpecId, DeliveryStatus, GitHubPullRequestTargetRef, RepositoryRef,
    },
    store::{
        AppendDelivery, CreateDelivery, DeliveryCommand, DeliveryCommandPort,
        DeliveryMutationOperation, DeliveryQuery, DeliveryQueryPort, DeliveryStore,
        DeliveryStoreError, DeliveryStoreErrorCode, DeliveryStoreMutationResult,
        ResolveDeliveryAttention, StartDeliveryStage,
    },
};
use winwincode_domain::{DeliveryId, Sha256Digest};
use winwincode_storage::{CommitReceipt, ProductStateStorage, ReceiptIdentity, StorageError};

use crate::{
    DeliveryChangeKind, DeliveryCommandCommitError, StateChange, command_receipt,
    delivery_changed_event,
    delivery_transaction::{StagedDeliveryJournal, delivery_journal_key, delivery_stream_id},
    storage_commit, validate_delivery_changed_receipt,
};

const CROCKFORD_BASE32: &[u8; 32] = b"0123456789ABCDEFGHJKMNPQRSTVWXYZ";

/// Opaque product-owned semantic facts omitted from the public Spec command.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DeliverySpecFacts {
    now_millis: u64,
    repository: RepositoryRef,
    source_ref: Option<DeliverySourceRef>,
    scope: Vec<String>,
    out_of_scope: Vec<String>,
    constraints: Vec<String>,
    max_rework_attempts: u64,
    criterion_verification_methods: Vec<(String, String)>,
}

pub(crate) struct TrustedDeliverySpecFacts {
    pub(crate) now_millis: u64,
    pub(crate) repository: RepositoryRef,
    pub(crate) source_ref: Option<DeliverySourceRef>,
    pub(crate) scope: Vec<String>,
    pub(crate) out_of_scope: Vec<String>,
    pub(crate) constraints: Vec<String>,
    pub(crate) max_rework_attempts: u64,
    pub(crate) criterion_verification_methods: Vec<(String, String)>,
}

impl DeliverySpecFacts {
    pub(crate) fn from_trusted_adapter(facts: TrustedDeliverySpecFacts) -> Self {
        Self {
            now_millis: facts.now_millis,
            repository: facts.repository,
            source_ref: facts.source_ref,
            scope: facts.scope,
            out_of_scope: facts.out_of_scope,
            constraints: facts.constraints,
            max_rework_attempts: facts.max_rework_attempts,
            criterion_verification_methods: facts.criterion_verification_methods,
        }
    }
}

/// Trusted product facts required to turn a transport Delivery command into a
/// domain transition.
///
/// Fields and production construction stay inside the Control Plane so a
/// caller cannot turn a wall clock, repository ID, source binding, or stage
/// identity from the request body into product authority.
#[derive(Clone, Debug, PartialEq)]
pub struct DeliveryCommandFacts {
    command_digest: Sha256Digest,
    repository_scope: RepositoryScope,
    repository: RepositoryRef,
    source_ref: Option<DeliverySourceRef>,
    authority: DeliveryCommandAuthority,
}

#[derive(Clone, Debug, PartialEq)]
enum DeliveryCommandAuthority {
    Specification(Box<DeliverySpecFacts>),
    HumanAdvance(Box<StageAdvanceResult>),
    ResolveAttention(Box<ResolvedAttentionTransition>),
}

impl DeliveryCommandFacts {
    pub(crate) fn specification_from_trusted_adapter(
        command: &CommandEnvelope,
        repository_scope: RepositoryScope,
        facts: DeliverySpecFacts,
    ) -> Result<Self, StorageError> {
        Self::from_trusted_adapter(
            command,
            repository_scope,
            facts.repository.clone(),
            facts.source_ref.clone(),
            DeliveryCommandAuthority::Specification(Box::new(facts)),
        )
    }

    pub(crate) fn advance_from_trusted_adapter(
        command: &CommandEnvelope,
        repository_scope: RepositoryScope,
        repository: RepositoryRef,
        source_ref: Option<DeliverySourceRef>,
        transition: StageAdvanceResult,
    ) -> Result<Self, StorageError> {
        Self::from_trusted_adapter(
            command,
            repository_scope,
            repository,
            source_ref,
            DeliveryCommandAuthority::HumanAdvance(Box::new(transition)),
        )
    }

    pub(crate) fn attention_from_trusted_adapter(
        command: &CommandEnvelope,
        repository_scope: RepositoryScope,
        repository: RepositoryRef,
        source_ref: Option<DeliverySourceRef>,
        transition: ResolvedAttentionTransition,
    ) -> Result<Self, StorageError> {
        Self::from_trusted_adapter(
            command,
            repository_scope,
            repository,
            source_ref,
            DeliveryCommandAuthority::ResolveAttention(Box::new(transition)),
        )
    }

    fn from_trusted_adapter(
        command: &CommandEnvelope,
        repository_scope: RepositoryScope,
        repository: RepositoryRef,
        source_ref: Option<DeliverySourceRef>,
        authority: DeliveryCommandAuthority,
    ) -> Result<Self, StorageError> {
        let Scope::RepositoryScope(command_scope) = &command.scope else {
            return Err(StorageError::invalid_input(
                "Delivery command facts require repository scope",
            ));
        };
        if command_scope != &repository_scope {
            return Err(StorageError::invalid_input(
                "trusted repository authority does not match the command repository scope",
            ));
        }
        let (_, command_digest) = command_receipt(command)?;
        Ok(Self {
            command_digest,
            repository_scope,
            repository,
            source_ref,
            authority,
        })
    }

    fn validate_for(
        &self,
        scope: &RepositoryScope,
        command_digest: &Sha256Digest,
    ) -> Result<(), StorageError> {
        if &self.repository_scope != scope || &self.command_digest != command_digest {
            return Err(StorageError::invalid_input(
                "trusted Delivery command facts do not match the exact command and repository scope",
            ));
        }
        Ok(())
    }

    fn specification(&self) -> Result<&DeliverySpecFacts, StorageError> {
        let DeliveryCommandAuthority::Specification(facts) = &self.authority else {
            return Err(StorageError::invalid_input(
                "Delivery command facts do not contain sealed Spec authority",
            ));
        };
        Ok(facts)
    }

    fn human_advance(&self) -> Result<&StageAdvanceResult, StorageError> {
        let DeliveryCommandAuthority::HumanAdvance(transition) = &self.authority else {
            return Err(StorageError::invalid_input(
                "Delivery command facts do not contain a sealed human stage transition",
            ));
        };
        Ok(transition)
    }

    fn attention_resolution(&self) -> Result<&ResolvedAttentionTransition, StorageError> {
        let DeliveryCommandAuthority::ResolveAttention(transition) = &self.authority else {
            return Err(StorageError::invalid_input(
                "Delivery command facts do not contain a sealed Attention transition",
            ));
        };
        Ok(transition)
    }
}

pub(crate) fn execute(
    storage: &mut dyn ProductStateStorage,
    command: &CommandEnvelope,
    facts: &DeliveryCommandFacts,
) -> Result<CommitReceipt, DeliveryCommandCommitError> {
    if !matches!(
        command.command,
        CommandName::DeliveryCreate
            | CommandName::DeliveryUpdateSpec
            | CommandName::DeliveryAdvance
            | CommandName::DeliveryResolveAttention
    ) {
        return Err(StorageError::invalid_input(
            "base Delivery transaction does not own this Delivery command",
        )
        .into());
    }
    let Scope::RepositoryScope(scope) = &command.scope else {
        return Err(
            StorageError::invalid_input("Delivery commands require repository scope").into(),
        );
    };
    let (receipt_identity, command_digest) = command_receipt(command)?;
    let prior_receipt = storage.load_receipt(&receipt_identity, &command_digest)?;
    if let Some(receipt) = prior_receipt {
        validate_replayed_receipt(&receipt, &receipt_identity, command)?;
        return Ok(receipt);
    }
    facts.validate_for(scope, &command_digest)?;
    let parsed = parse_command(command)?;

    match parsed {
        ParsedCommand::Create(payload) => create(
            storage,
            command,
            scope,
            payload,
            &receipt_identity,
            &command_digest,
            facts,
        ),
        ParsedCommand::UpdateSpec(payload) => update_spec(
            storage,
            command,
            scope,
            payload,
            &receipt_identity,
            &command_digest,
            facts,
        )
        .map_err(Into::into),
        ParsedCommand::Advance(payload) => advance_human_stage(
            storage,
            command,
            &payload,
            &receipt_identity,
            &command_digest,
            facts,
        )
        .map_err(Into::into),
        ParsedCommand::ResolveAttention(payload) => resolve_business_attention(
            storage,
            command,
            &payload,
            &receipt_identity,
            &command_digest,
            facts,
        )
        .map_err(Into::into),
    }
}

enum ParsedCommand {
    Create(DeliveryCreatePayload),
    UpdateSpec(DeliveryUpdateSpecPayload),
    Advance(DeliveryAdvancePayload),
    ResolveAttention(DeliveryResolveAttentionPayload),
}

fn parse_command(command: &CommandEnvelope) -> Result<ParsedCommand, StorageError> {
    match command.command {
        CommandName::DeliveryCreate => Ok(ParsedCommand::Create(strict_payload::<
            DeliveryCreatePayload,
        >(
            command, "delivery.create"
        )?)),
        CommandName::DeliveryUpdateSpec => Ok(ParsedCommand::UpdateSpec(strict_payload::<
            DeliveryUpdateSpecPayload,
        >(
            command,
            "delivery.update_spec",
        )?)),
        CommandName::DeliveryAdvance => Ok(ParsedCommand::Advance(strict_payload::<
            DeliveryAdvancePayload,
        >(
            command, "delivery.advance"
        )?)),
        CommandName::DeliveryResolveAttention => {
            Ok(ParsedCommand::ResolveAttention(strict_payload::<
                DeliveryResolveAttentionPayload,
            >(
                command,
                "delivery.resolve_attention",
            )?))
        }
        _ => Err(StorageError::invalid_input(
            "base Delivery transaction does not own this Delivery command",
        )),
    }
}

fn create(
    storage: &mut dyn ProductStateStorage,
    command: &CommandEnvelope,
    scope: &RepositoryScope,
    payload: DeliveryCreatePayload,
    receipt_identity: &ReceiptIdentity,
    command_digest: &Sha256Digest,
    facts: &DeliveryCommandFacts,
) -> Result<CommitReceipt, DeliveryCommandCommitError> {
    let spec_facts = facts.specification()?;
    if command.expected_revision.0 != 0 {
        return Err(StorageError::revision_conflict(
            u64::try_from(command.expected_revision.0).unwrap_or(u64::MAX),
            0,
        )
        .into());
    }
    if payload.spec.repository_id != scope.repository_id {
        return Err(StorageError::invalid_input(
            "delivery.create repositoryId does not match command repository scope",
        )
        .into());
    }
    if !payload.tasks.is_empty() {
        return Err(StorageError::invalid_input("delivery.create.tasks must be empty").into());
    }
    validate_create_tasks_empty(&[]).map_err(storage_error)?;
    if storage
        .load_journal(&delivery_journal_key(&payload.delivery_id)?)?
        .is_some()
    {
        return Err(DeliveryCommandCommitError::AlreadyExists {
            delivery_id: payload.delivery_id,
        });
    }
    let snapshot = DeliverySnapshot {
        schema_version: DELIVERY_SCHEMA_VERSION,
        id: payload.delivery_id.clone(),
        revision: 1,
        status: DeliveryStatus::Draft,
        spec: map_spec(
            &payload.delivery_id,
            1,
            payload.spec,
            spec_facts,
            command_digest,
        )?,
        tasks: Vec::new(),
        stage_runs: Vec::new(),
        session_bindings: Vec::new(),
        attention_items: Vec::new(),
        evidence: Vec::new(),
        verdict: None,
        created_at_millis: spec_facts.now_millis,
        updated_at_millis: spec_facts.now_millis,
    };
    let delivery = Delivery::try_from_snapshot(snapshot).map_err(storage_error)?;
    let request_digest = raw_digest(command_digest)?;
    let journal = StagedDeliveryJournal::new(payload.delivery_id.clone(), None);
    let mutation = DeliveryStore::borrowed(&journal)
        .execute(DeliveryCommand::Create(CreateDelivery {
            request_id: command.request_id.clone(),
            request_digest,
            snapshot: delivery,
        }))
        .map_err(|error| {
            if error.code() == DeliveryStoreErrorCode::DeliveryAlreadyExists {
                DeliveryCommandCommitError::AlreadyExists {
                    delivery_id: payload.delivery_id.clone(),
                }
            } else {
                delivery_store_error(&error, &command.request_id).into()
            }
        })?;
    if mutation.replayed {
        return Err(StorageError::invalid_input(
            "Delivery journal replay is missing its scoped command receipt",
        )
        .into());
    }
    match commit_mutation(
        storage,
        command,
        receipt_identity,
        &payload.delivery_id,
        journal,
        &mutation,
        DeliveryChangeKind::Created,
    ) {
        Ok(receipt) => Ok(receipt),
        Err(error)
            if matches!(
                error.kind(),
                winwincode_storage::StorageErrorKind::RevisionConflict
                    | winwincode_storage::StorageErrorKind::JournalAlreadyExists
            ) =>
        {
            Err(DeliveryCommandCommitError::AlreadyExists {
                delivery_id: payload.delivery_id,
            })
        }
        Err(error) => Err(error.into()),
    }
}

fn update_spec(
    storage: &mut dyn ProductStateStorage,
    command: &CommandEnvelope,
    scope: &RepositoryScope,
    payload: DeliveryUpdateSpecPayload,
    receipt_identity: &ReceiptIdentity,
    command_digest: &Sha256Digest,
    facts: &DeliveryCommandFacts,
) -> Result<CommitReceipt, StorageError> {
    let spec_facts = facts.specification()?;
    if payload.spec.repository_id != scope.repository_id {
        return Err(StorageError::invalid_input(
            "delivery.update_spec repositoryId does not match command repository scope",
        ));
    }
    let expected_revision = expected_revision(command)?;
    let journal_key = delivery_journal_key(&payload.delivery_id)?;
    let loaded = storage.load_journal(&journal_key)?;
    let journal = StagedDeliveryJournal::new(payload.delivery_id.clone(), loaded);
    let current = DeliveryStore::borrowed(&journal)
        .query(DeliveryQuery::Get(payload.delivery_id.clone()))
        .map_err(|error| delivery_store_error(&error, &command.request_id))?;
    validate_repository_scope(
        &current,
        &spec_facts.repository,
        spec_facts.source_ref.as_ref(),
    )?;
    if current.revision() != expected_revision {
        return Err(StorageError::revision_conflict(
            expected_revision,
            current.revision(),
        ));
    }
    let input_target = map_publication_target(payload.spec.publication_target.as_ref());
    if input_target != current.snapshot().spec.publication_target {
        return Err(StorageError::invalid_input(
            "delivery.update_spec cannot replace the canonical publication target",
        ));
    }
    if spec_facts.source_ref != current.snapshot().spec.source_ref {
        return Err(StorageError::invalid_input(
            "trusted Delivery source binding changed before Spec replacement",
        ));
    }
    let mut snapshot = current.clone().into_snapshot();
    snapshot.revision = expected_revision.saturating_add(1);
    snapshot.status = DeliveryStatus::Ready;
    snapshot.spec = map_spec(
        &payload.delivery_id,
        current.snapshot().spec.revision.saturating_add(1),
        payload.spec,
        spec_facts,
        command_digest,
    )?;
    snapshot.tasks.clear();
    snapshot.stage_runs.clear();
    snapshot.session_bindings.clear();
    snapshot.attention_items.clear();
    snapshot.evidence.clear();
    snapshot.verdict = None;
    snapshot.updated_at_millis = spec_facts.now_millis;
    let replacement = Delivery::try_from_snapshot(snapshot).map_err(storage_error)?;
    let mutation = DeliveryStore::borrowed(&journal)
        .execute(DeliveryCommand::Append(AppendDelivery {
            delivery_id: payload.delivery_id.clone(),
            request_id: command.request_id.clone(),
            request_digest: raw_digest(command_digest)?,
            operation: DeliveryMutationOperation::DeliverySpecUpdated,
            expected_revision,
            snapshot: replacement,
        }))
        .map_err(|error| delivery_store_error(&error, &command.request_id))?;
    commit_mutation(
        storage,
        command,
        receipt_identity,
        &payload.delivery_id,
        journal,
        &mutation,
        DeliveryChangeKind::Advanced,
    )
}

fn advance_human_stage(
    storage: &mut dyn ProductStateStorage,
    command: &CommandEnvelope,
    payload: &DeliveryAdvancePayload,
    receipt_identity: &ReceiptIdentity,
    command_digest: &Sha256Digest,
    facts: &DeliveryCommandFacts,
) -> Result<CommitReceipt, StorageError> {
    let expected_revision = expected_revision(command)?;
    let (journal, current) = load_current(storage, &payload.delivery_id, command)?;
    if current.revision() != expected_revision {
        return Err(StorageError::revision_conflict(
            expected_revision,
            current.revision(),
        ));
    }
    let transition = facts.human_advance()?;
    transition
        .validate_projection()
        .map_err(|error| coordination_error(&error, expected_revision, current.revision()))?;
    let StageAdvanceEffect::Review(attention_item_id) = &transition.effect else {
        return Err(StorageError::invalid_input(
            "delivery.advance selected a Codex effect that requires the typed execution transaction",
        ));
    };
    let result = &transition.delivery;
    validate_repository_scope(&current, &facts.repository, facts.source_ref.as_ref())?;
    let attention = result
        .snapshot()
        .attention_items
        .iter()
        .find(|item| item.id == *attention_item_id)
        .ok_or_else(|| {
            StorageError::invalid_input(
                "sealed human stage transition has no exact review Attention",
            )
        })?;
    if result.id() != current.id()
        || result.revision() != expected_revision.saturating_add(1)
        || result.snapshot().spec.repository != facts.repository
        || result.snapshot().spec.source_ref != facts.source_ref
        || current
            .snapshot()
            .attention_items
            .iter()
            .any(|item| item.id == *attention_item_id)
        || attention.assigned_to.as_deref() != Some(actor_id(&command.actor).as_str())
    {
        return Err(StorageError::invalid_input(
            "sealed human stage transition does not match command actor, Delivery, or revision",
        ));
    }
    let mutation = DeliveryStore::borrowed(&journal)
        .execute(DeliveryCommand::StartStage(Box::new(StartDeliveryStage {
            request_id: command.request_id.clone(),
            request_digest: raw_digest(command_digest)?,
            expected_revision,
            transition: transition.clone(),
        })))
        .map_err(|error| delivery_store_error(&error, &command.request_id))?;
    commit_mutation(
        storage,
        command,
        receipt_identity,
        &payload.delivery_id,
        journal,
        &mutation,
        DeliveryChangeKind::Advanced,
    )
}

fn resolve_business_attention(
    storage: &mut dyn ProductStateStorage,
    command: &CommandEnvelope,
    payload: &DeliveryResolveAttentionPayload,
    receipt_identity: &ReceiptIdentity,
    command_digest: &Sha256Digest,
    facts: &DeliveryCommandFacts,
) -> Result<CommitReceipt, StorageError> {
    if payload.remediation.is_some() {
        return Err(StorageError::invalid_input(
            "delivery.resolve_attention remediation requires its sealed remediation transaction",
        ));
    }
    let expected_revision = expected_revision(command)?;
    let (journal, current) = load_current(storage, &payload.delivery_id, command)?;
    if current.revision() != expected_revision {
        return Err(StorageError::revision_conflict(
            expected_revision,
            current.revision(),
        ));
    }
    let source_item = current
        .snapshot()
        .attention_items
        .iter()
        .find(|item| item.id == payload.attention_item_id)
        .ok_or_else(|| {
            StorageError::invalid_input(
                "AttentionItem does not belong to the current scoped Delivery",
            )
        })?;
    let stage_run_id = source_item.stage_run_id.clone().ok_or_else(|| {
        StorageError::invalid_input("business Attention must reference its current StageRun")
    })?;
    let expected_status = match payload.decision.as_str() {
        "resolve" => AttentionItemStatus::Resolved,
        "dismiss" => AttentionItemStatus::Dismissed,
        _ => {
            return Err(StorageError::invalid_input(
                "delivery.resolve_attention decision is not canonical",
            ));
        }
    };
    let transition = facts.attention_resolution()?;
    let result = transition.delivery();
    validate_repository_scope(&current, &facts.repository, facts.source_ref.as_ref())?;
    let resolved = result
        .snapshot()
        .attention_items
        .iter()
        .find(|item| item.id == payload.attention_item_id)
        .ok_or_else(|| {
            StorageError::invalid_input(
                "sealed Attention transition has no exact resolved AttentionItem",
            )
        })?;
    let actor = actor_id(&command.actor);
    if result.id() != current.id()
        || result.revision() != expected_revision.saturating_add(1)
        || result.snapshot().spec.repository != facts.repository
        || result.snapshot().spec.source_ref != facts.source_ref
        || resolved.stage_run_id.as_ref() != Some(&stage_run_id)
        || resolved.status != expected_status
        || resolved.resolution.as_deref() != Some(payload.resolution.as_str())
        || resolved.resolved_by.as_deref() != Some(actor.as_str())
        || resolved.resolved_at_millis.is_none()
        || resolved.resolved_at_millis != Some(result.snapshot().updated_at_millis)
    {
        return Err(StorageError::invalid_input(
            "sealed Attention transition does not match command target, actor, decision, resolution, or revision",
        ));
    }
    let mutation = DeliveryStore::borrowed(&journal)
        .execute(DeliveryCommand::ResolveAttention(Box::new(
            ResolveDeliveryAttention {
                request_id: command.request_id.clone(),
                request_digest: raw_digest(command_digest)?,
                expected_revision,
                transition: transition.clone(),
            },
        )))
        .map_err(|error| delivery_store_error(&error, &command.request_id))?;
    commit_mutation(
        storage,
        command,
        receipt_identity,
        &payload.delivery_id,
        journal,
        &mutation,
        DeliveryChangeKind::Advanced,
    )
}

fn load_current(
    storage: &dyn ProductStateStorage,
    delivery_id: &DeliveryId,
    command: &CommandEnvelope,
) -> Result<(StagedDeliveryJournal, Delivery), StorageError> {
    let journal_key = delivery_journal_key(delivery_id)?;
    let loaded = storage.load_journal(&journal_key)?;
    let journal = StagedDeliveryJournal::new(delivery_id.clone(), loaded);
    let current = DeliveryStore::borrowed(&journal)
        .query(DeliveryQuery::Get(delivery_id.clone()))
        .map_err(|error| delivery_store_error(&error, &command.request_id))?;
    Ok((journal, current))
}

fn actor_id(actor: &Actor) -> String {
    match actor {
        Actor::UserActor(actor) => actor.id.0.clone(),
        Actor::ServiceAccountActor(actor) => actor.id.0.clone(),
        Actor::SystemActor(actor) => actor.id.0.clone(),
    }
}

fn coordination_error(
    error: &CoordinationError,
    expected_revision: u64,
    current_revision: u64,
) -> StorageError {
    if error.code() == CoordinationErrorCode::RevisionConflict {
        StorageError::revision_conflict(expected_revision, current_revision)
    } else {
        StorageError::invalid_input(error.to_string())
    }
}

fn commit_mutation(
    storage: &mut dyn ProductStateStorage,
    command: &CommandEnvelope,
    receipt_identity: &ReceiptIdentity,
    delivery_id: &DeliveryId,
    journal: StagedDeliveryJournal,
    mutation: &DeliveryStoreMutationResult,
    change_kind: DeliveryChangeKind,
) -> Result<CommitReceipt, StorageError> {
    if mutation.replayed {
        return Err(StorageError::invalid_input(
            "Delivery journal replay is missing its scoped command receipt",
        ));
    }
    let changed_event = delivery_changed_event(
        command,
        delivery_id,
        mutation.snapshot.revision(),
        change_kind,
    )?;
    let mut commit = storage_commit(
        command,
        StateChange::new(
            delivery_stream_id(delivery_id),
            mutation.snapshot.encode_json().map_err(storage_error)?,
            vec![changed_event],
        ),
    )?;
    let publication = journal
        .into_publication()
        .map_err(storage_error)?
        .ok_or_else(|| {
            StorageError::invalid_input("new Delivery mutation did not stage a journal record")
        })?;
    commit = commit.with_journal_publication(publication);
    let receipt = storage.commit(&commit)?;
    validate_receipt(
        &receipt,
        receipt_identity,
        delivery_id,
        command,
        change_kind,
        receipt.idempotent_replay,
    )?;
    Ok(receipt)
}

fn map_spec(
    delivery_id: &DeliveryId,
    revision: u64,
    input: DeliverySpecInput,
    facts: &DeliverySpecFacts,
    command_digest: &Sha256Digest,
) -> Result<DeliverySpec, StorageError> {
    let publication_target =
        input
            .publication_target
            .as_ref()
            .map(|target| GitHubPullRequestTargetRef {
                schema_version: DELIVERY_SCHEMA_VERSION,
                provider: "github".to_owned(),
                kind: "pull-request".to_owned(),
                repository: target.repository.0.clone(),
                base_branch: target.base_branch.clone(),
                head_repository: target.head_repository.0.clone(),
                head_branch: target.head_branch.clone(),
            });
    if facts.criterion_verification_methods.len() != input.acceptance_criteria.len() {
        return Err(StorageError::invalid_input(
            "trusted Spec facts must provide one verification method for every exact acceptance criterion",
        ));
    }
    let criteria = input
        .acceptance_criteria
        .into_iter()
        .zip(&facts.criterion_verification_methods)
        .map(|(criterion, (criterion_id, verification_method))| {
            if criterion.id != *criterion_id || verification_method.trim().is_empty() {
                return Err(StorageError::invalid_input(
                    "trusted Spec facts must match acceptance criterion IDs in exact order",
                ));
            }
            Ok(AcceptanceCriterion {
                schema_version: DELIVERY_SCHEMA_VERSION,
                id: AcceptanceCriterionId::new(criterion.id).map_err(storage_error)?,
                description: criterion.title,
                verification_method: Some(verification_method.clone()),
                required: criterion.required,
            })
        })
        .collect::<Result<Vec<_>, StorageError>>()?;
    Ok(DeliverySpec {
        schema_version: DELIVERY_SCHEMA_VERSION,
        id: DeliverySpecId::new(derived_portable_id(
            "delivery-spec",
            revision,
            command_digest,
        ))
        .map_err(storage_error)?,
        delivery_id: delivery_id.clone(),
        revision,
        title: input.title,
        goal: input.goal,
        scope: facts.scope.clone(),
        out_of_scope: facts.out_of_scope.clone(),
        constraints: facts.constraints.clone(),
        acceptance_criteria: criteria,
        source_ref: facts.source_ref.clone(),
        publication_target,
        repository: facts.repository.clone(),
        base_revision: input.base_revision,
        max_rework_attempts: facts.max_rework_attempts,
        created_at_millis: facts.now_millis,
    })
}

fn map_publication_target(
    target: Option<&winwincode_api::generated::PublicationTarget>,
) -> Option<DeliveryPublicationTarget> {
    target.map(|target| GitHubPullRequestTargetRef {
        schema_version: DELIVERY_SCHEMA_VERSION,
        provider: "github".to_owned(),
        kind: "pull-request".to_owned(),
        repository: target.repository.0.clone(),
        base_branch: target.base_branch.clone(),
        head_repository: target.head_repository.0.clone(),
        head_branch: target.head_branch.clone(),
    })
}

fn validate_repository_scope(
    delivery: &Delivery,
    trusted_repository: &RepositoryRef,
    source_ref: Option<&DeliverySourceRef>,
) -> Result<(), StorageError> {
    let repository = &delivery.snapshot().spec.repository;
    if repository != trusted_repository
        || delivery.snapshot().spec.source_ref.as_ref() != source_ref
    {
        return Err(StorageError::invalid_input(
            "Delivery does not match the trusted repository and source binding",
        ));
    }
    Ok(())
}

fn expected_revision(command: &CommandEnvelope) -> Result<u64, StorageError> {
    u64::try_from(command.expected_revision.0)
        .map_err(|_| StorageError::invalid_input("Delivery expectedRevision must not be negative"))
}

fn strict_payload<T>(command: &CommandEnvelope, name: &str) -> Result<T, StorageError>
where
    T: serde::de::DeserializeOwned + serde::Serialize,
{
    let payload: T = serde_json::from_value(command.payload.clone()).map_err(|error| {
        StorageError::invalid_input(format!("{name} payload is not canonical: {error}"))
    })?;
    if serde_json::to_value(&payload).map_err(storage_error)? != command.payload {
        return Err(StorageError::invalid_input(format!(
            "{name} payload is not canonical"
        )));
    }
    Ok(payload)
}

fn validate_receipt(
    receipt: &CommitReceipt,
    expected_identity: &ReceiptIdentity,
    delivery_id: &DeliveryId,
    command: &CommandEnvelope,
    change_kind: DeliveryChangeKind,
    expected_replay: bool,
) -> Result<(), StorageError> {
    let expected_revision = u64::try_from(command.expected_revision.0)
        .map_err(|_| StorageError::invalid_input("Delivery expectedRevision must not be negative"))?
        .saturating_add(1);
    if &receipt.receipt_identity != expected_identity
        || receipt.stream_id != delivery_stream_id(delivery_id)
        || receipt.revision != expected_revision
        || receipt.idempotent_replay != expected_replay
        || receipt.events.len() != 1
    {
        return Err(StorageError::invalid_input(
            "durable Delivery command receipt does not match its exact scoped result",
        ));
    }
    validate_delivery_changed_receipt(receipt, delivery_id, expected_revision, change_kind)
}

fn validate_replayed_receipt(
    receipt: &CommitReceipt,
    expected_identity: &ReceiptIdentity,
    command: &CommandEnvelope,
) -> Result<(), StorageError> {
    let expected_change_kind = match command.command {
        CommandName::DeliveryCreate => DeliveryChangeKind::Created,
        CommandName::DeliveryUpdateSpec
        | CommandName::DeliveryAdvance
        | CommandName::DeliveryResolveAttention => DeliveryChangeKind::Advanced,
        _ => {
            return Err(StorageError::invalid_input(
                "base Delivery transaction does not own this Delivery command",
            ));
        }
    };
    if &receipt.receipt_identity != expected_identity
        || !receipt.idempotent_replay
        || receipt.events.len() != 1
    {
        return Err(StorageError::invalid_input(
            "durable Delivery command receipt does not match its exact scoped result",
        ));
    }
    let event: ControlPlaneWebSocketDeliveryChangedEvent =
        serde_json::from_slice(&receipt.events[0].payload).map_err(|_| {
            StorageError::invalid_input("durable Delivery change event is not canonical")
        })?;
    let event_revision = u64::try_from(event.revision.0).map_err(|_| {
        StorageError::invalid_input("durable Delivery change event revision is not positive")
    })?;
    if event.change_kind != expected_change_kind.as_str()
        || event_revision == 0
        || receipt.revision != event_revision
        || receipt.stream_id != delivery_stream_id(&event.delivery_id)
    {
        return Err(StorageError::invalid_input(
            "durable Delivery command receipt does not match its exact command kind",
        ));
    }
    validate_delivery_changed_receipt(
        receipt,
        &event.delivery_id,
        event_revision,
        expected_change_kind,
    )
}

fn raw_digest(digest: &Sha256Digest) -> Result<String, StorageError> {
    digest
        .0
        .strip_prefix("sha256:")
        .map(str::to_owned)
        .ok_or_else(|| StorageError::invalid_input("command digest is not canonical"))
}

fn derived_portable_id(prefix: &str, revision: u64, digest: &Sha256Digest) -> String {
    let mut hasher = Sha256::new();
    hasher.update(b"winwincode.delivery-command-authority.v1\0");
    hasher.update(prefix.as_bytes());
    hasher.update(revision.to_be_bytes());
    hasher.update(digest.0.as_bytes());
    let digest = hasher.finalize();
    let suffix = digest
        .iter()
        .take(26)
        .map(|byte| char::from(CROCKFORD_BASE32[usize::from(byte & 31)]))
        .collect::<String>();
    format!("{prefix}-{suffix}")
}

fn delivery_store_error(
    error: &DeliveryStoreError,
    request_id: &winwincode_domain::RequestId,
) -> StorageError {
    if matches!(
        error.code(),
        DeliveryStoreErrorCode::StoreCorrupt | DeliveryStoreErrorCode::StoreIoError
    ) {
        return StorageError::adapter(error.to_string());
    }
    match error.code() {
        DeliveryStoreErrorCode::RevisionConflict => {
            if let (Some(expected), Some(current)) =
                (error.expected_revision(), error.current_revision())
            {
                return StorageError::revision_conflict(expected, current);
            }
        }
        DeliveryStoreErrorCode::RequestConflict => {
            return StorageError::request_conflict(request_id);
        }
        DeliveryStoreErrorCode::ReviewSetStale
        | DeliveryStoreErrorCode::InvalidStoreOptions
        | DeliveryStoreErrorCode::DeliveryAlreadyExists
        | DeliveryStoreErrorCode::DeliveryNotFound
        | DeliveryStoreErrorCode::StoreCorrupt
        | DeliveryStoreErrorCode::DeliveryIdMismatch
        | DeliveryStoreErrorCode::StoreIoError => {}
    }
    StorageError::invalid_input(error.to_string())
}

fn storage_error(error: impl std::fmt::Display) -> StorageError {
    StorageError::invalid_input(error.to_string())
}
