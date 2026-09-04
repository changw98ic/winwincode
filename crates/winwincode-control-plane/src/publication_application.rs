// SPDX-License-Identifier: Apache-2.0

//! Generated Publication command/query application boundary.

use base64::{Engine as _, engine::general_purpose::URL_SAFE_NO_PAD};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use winwincode_api::generated::{
    Actor, ActorId, CommandEnvelope, CommandName, PageInfo, PublicationCancelCommand,
    PublicationCancelCompletedResponse, PublicationCancelCompletedResponseCommand,
    PublicationCancelCompletedResponseOutcome, PublicationGetQuery, PublicationGetResultResponse,
    PublicationGetResultResponseQuery, PublicationListQuery, PublicationListResultResponse,
    PublicationListResultResponseQuery, PublicationPage, PublicationPageKind,
    PublicationProjection, PublicationProjectionVerdictStatus,
    PublicationResourceKind as ApiPublicationResourceKind, PublicationResourceRef,
    PublicationTarget as ApiPublicationTarget, PublicationTargetProvider, Scope,
};
use winwincode_audit::{
    AuditAction, AuditActor, AuditEvent, AuditEventId, AuditOrigin, AuditRetention, AuditScope,
    AuditState, AuditSubject,
};
use winwincode_domain::RepositoryScope;
use winwincode_domain::{
    GitHubRepositorySlug, OpaqueCursor, PublicationId, Revision, SchemaVersion, Sha256Digest,
    UserId,
};
use winwincode_publication::{
    Publication, PublicationCancelCommand as DomainCancelCommand, PublicationCommandContext,
    PublicationCoordinator, PublicationLedger, PublicationOperation, PublicationPolicyAudit,
    PublicationPolicyAuditError, PublicationPolicyOrigin, PublicationPort, PublicationPortError,
    PublicationPortMutation, PublicationPortObservation, PublicationResourceKind, PublicationState,
    RepositoryPolicyScope,
};
use winwincode_storage::{ProductStateStorage, StorageError};

use crate::{
    ControlPlane, CredentialLeakGate, CredentialOutputBoundary, PublicationCommandError,
    command_receipt, command_receipt_identity, instant_from_millis,
};

const PUBLICATION_STREAM_PREFIX: &str = "publication:";
const CURSOR_SCHEMA: &str = "winwincode.publication-list.cursor.v1";
const MAX_CURSOR_BYTES: usize = 4_096;
const MAX_PAGE_SIZE: usize = 200;
const SCAN_PAGE_SIZE: usize = 256;
const MAX_SNAPSHOT_ROWS: usize = 100_000;

#[derive(Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct PublicationListCursor {
    schema: String,
    scope_sha256: String,
    filter_sha256: String,
    upper_bound_stream_id: String,
    snapshot_sha256: String,
    after_publication_id: PublicationId,
}

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct PublicationListFilter<'query> {
    delivery_id: Option<&'query winwincode_domain::DeliveryId>,
    states: &'query [String],
}

struct LoadedPublicationPage {
    items: Vec<PublicationProjection>,
    has_more: bool,
    snapshot_sha256: String,
    after_seen: bool,
}

impl ControlPlane {
    /// Cancels one scoped Publication without invoking a provider operation.
    /// The exact generated command receipt and two deterministic audit facts
    /// make both a successful first attempt and a restart replay observable.
    ///
    /// # Errors
    ///
    /// Rejects a foreign scope, changed request body, stale revision, terminal
    /// state, invalid time, unavailable audit store, or corrupt durable state.
    pub fn publication_cancel(
        &mut self,
        command: &PublicationCancelCommand,
        occurred_at_millis: u64,
    ) -> Result<PublicationCancelCompletedResponse, PublicationCommandError> {
        let origin = PublicationPolicyOrigin::local("control-plane-http")
            .map_err(PublicationCommandError::InvalidInput)?;
        let mapped = mapped_cancel(command, occurred_at_millis)?;
        if self.audit_store.is_none() {
            return Err(PublicationCommandError::AuditUnavailable(
                winwincode_audit::AuditError::unavailable(),
            ));
        }

        let current = load_publication(
            self.storage_mut().map_err(publication_storage_error)?,
            &command.payload.publication_id,
        )?;
        ensure_scope(&command.scope, &current)?;
        let audit_occurred_at = cancel_audit_occurred_at(
            self.audit_store
                .as_ref()
                .ok_or_else(winwincode_audit::AuditError::unavailable)?,
            command,
            mapped.context.command_digest(),
            occurred_at_millis,
        )?;
        append_cancel_audit(
            self.audit_store
                .as_mut()
                .ok_or_else(winwincode_audit::AuditError::unavailable)?,
            command,
            mapped.context.command_digest(),
            &current,
            &origin,
            audit_occurred_at,
            CancelAuditPhase::Requested,
        )?;

        let publication = cancel_publication(
            self.storage_mut().map_err(publication_storage_error)?,
            &mapped.context,
            &mapped.command,
        )?;
        ensure_scope(&command.scope, &publication)?;
        append_cancel_audit(
            self.audit_store
                .as_mut()
                .ok_or_else(winwincode_audit::AuditError::unavailable)?,
            command,
            mapped.context.command_digest(),
            &publication,
            &origin,
            publication.updated_at_millis(),
            CancelAuditPhase::Completed,
        )?;

        if matches!(
            publication.state(),
            PublicationState::Published | PublicationState::Cancelled | PublicationState::Failed
        ) {
            self.finalize_candidate_git_for_terminal_delivery(publication.binding().delivery_id())
                .map_err(|error| {
                    PublicationCommandError::Publication(
                        winwincode_publication::PublicationError::from(StorageError::adapter(
                            error.to_string(),
                        )),
                    )
                })?;
        }

        let result = publication_projection(&publication)?;
        checked_response(PublicationCancelCompletedResponse {
            command: PublicationCancelCompletedResponseCommand::PublicationCancel,
            current_revision: result.revision.clone(),
            outcome: PublicationCancelCompletedResponseOutcome::Completed,
            previous_revision: previous_revision(publication.revision())?,
            request_id: command.request_id.clone(),
            result,
            schema_version: SchemaVersion::WinwincodeV1,
        })
    }

    /// Reads one exact scoped Publication after validating its complete durable
    /// journal. No provider adapter is reachable from this query path.
    ///
    /// # Errors
    ///
    /// Rejects an invalid query, foreign scope, missing Publication, or corrupt
    /// state/journal pair.
    pub fn publication_get(
        &mut self,
        query: &PublicationGetQuery,
    ) -> Result<PublicationGetResultResponse, PublicationCommandError> {
        if query.schema_version != SchemaVersion::WinwincodeV1 {
            return Err(PublicationCommandError::InvalidInput(
                "publication.get query is invalid".to_owned(),
            ));
        }
        validate_query_identity(
            &query.actor,
            &query.scope,
            &query.request_id,
            query.page.limit,
        )?;
        if query.page.cursor.is_some() {
            return Err(PublicationCommandError::InvalidInput(
                "publication.get does not accept a page cursor".to_owned(),
            ));
        }
        let publication = load_publication(
            self.storage_mut().map_err(publication_storage_error)?,
            &query.parameters.publication_id,
        )?;
        ensure_scope(&query.scope, &publication)?;
        checked_response(PublicationGetResultResponse {
            page: PageInfo {
                has_more: false,
                next_cursor: None,
            },
            query: PublicationGetResultResponseQuery::PublicationGet,
            request_id: query.request_id.clone(),
            result: publication_projection(&publication)?,
            schema_version: SchemaVersion::WinwincodeV1,
        })
    }

    /// Lists scoped Publications through a bounded keyset walk frozen to an
    /// upper stream identity and a digest of every matching projection.
    ///
    /// # Errors
    ///
    /// Rejects invalid filters, foreign or stale cursors, unsupported storage
    /// enumeration, or any corrupt Publication state/journal pair.
    pub fn publication_list(
        &mut self,
        query: &PublicationListQuery,
    ) -> Result<PublicationListResultResponse, PublicationCommandError> {
        if query.schema_version != SchemaVersion::WinwincodeV1 {
            return Err(PublicationCommandError::InvalidInput(
                "publication.list query is invalid".to_owned(),
            ));
        }
        let limit = validate_query_identity(
            &query.actor,
            &query.scope,
            &query.request_id,
            query.page.limit,
        )?;
        let states = normalized_states(&query.parameters.states)?;
        let scope_sha256 = repository_scope(&query.scope)?.sha256().0;
        let filter_sha256 = digest_json(&PublicationListFilter {
            delivery_id: query.parameters.delivery_id.as_ref(),
            states: &states,
        })?;
        let decoded = decode_cursor(query.page.cursor.as_ref(), &scope_sha256, &filter_sha256)?;
        let upper_bound = match &decoded {
            Some(cursor) => Some(cursor.upper_bound_stream_id.clone()),
            None => self
                .storage_ref()
                .map_err(publication_storage_error)?
                .last_state_stream_id(PUBLICATION_STREAM_PREFIX)
                .map_err(publication_storage_error)?,
        };

        let page = upper_bound.as_deref().map_or_else(
            || {
                Ok(LoadedPublicationPage {
                    items: Vec::new(),
                    has_more: false,
                    snapshot_sha256: digest_bytes(b""),
                    after_seen: decoded.is_none(),
                })
            },
            |upper_bound| {
                load_publication_page(
                    self.storage_mut().map_err(publication_storage_error)?,
                    &query.scope,
                    query.parameters.delivery_id.as_ref(),
                    &states,
                    decoded.as_ref().map(|cursor| &cursor.after_publication_id),
                    upper_bound,
                    limit,
                )
            },
        )?;
        if let Some(cursor) = &decoded
            && (!page.after_seen || page.snapshot_sha256 != cursor.snapshot_sha256)
        {
            return Err(PublicationCommandError::ReadCursorExpired);
        }

        let next_cursor = if page.has_more {
            let after = page.items.last().ok_or_else(|| {
                PublicationCommandError::InvalidInput(
                    "publication list page has no keyset anchor".to_owned(),
                )
            })?;
            Some(encode_cursor(&PublicationListCursor {
                schema: CURSOR_SCHEMA.to_owned(),
                scope_sha256,
                filter_sha256,
                upper_bound_stream_id: upper_bound.ok_or_else(|| {
                    PublicationCommandError::InvalidInput(
                        "publication list upper bound is missing".to_owned(),
                    )
                })?,
                snapshot_sha256: page.snapshot_sha256,
                after_publication_id: after.id.clone(),
            })?)
        } else {
            None
        };
        checked_response(PublicationListResultResponse {
            page: PageInfo {
                has_more: page.has_more,
                next_cursor,
            },
            query: PublicationListResultResponseQuery::PublicationList,
            request_id: query.request_id.clone(),
            result: PublicationPage {
                items: page.items,
                kind: PublicationPageKind::PublicationPage,
            },
            schema_version: SchemaVersion::WinwincodeV1,
        })
    }
}

struct MappedCancel {
    context: PublicationCommandContext,
    command: DomainCancelCommand,
}

fn mapped_cancel(
    command: &PublicationCancelCommand,
    occurred_at_millis: u64,
) -> Result<MappedCancel, PublicationCommandError> {
    if command.schema_version != SchemaVersion::WinwincodeV1 {
        return Err(PublicationCommandError::InvalidInput(
            "publication.cancel command is invalid".to_owned(),
        ));
    }
    let expected_revision = u64::try_from(command.expected_revision.0).map_err(|_| {
        PublicationCommandError::InvalidInput(
            "publication.cancel expectedRevision is invalid".to_owned(),
        )
    })?;
    let generic = CommandEnvelope {
        actor: command.actor.clone(),
        command: CommandName::PublicationCancel,
        expected_revision: command.expected_revision.clone(),
        payload: serde_json::to_value(&command.payload).map_err(|_| {
            PublicationCommandError::InvalidInput(
                "publication.cancel payload cannot be encoded".to_owned(),
            )
        })?,
        request_id: command.request_id.clone(),
        schema_version: command.schema_version.clone(),
        scope: Scope::RepositoryScope(command.scope.clone()),
    };
    let (receipt_identity, command_digest) = command_receipt(&generic)
        .map_err(|error| PublicationCommandError::InvalidInput(error.to_string()))?;
    Ok(MappedCancel {
        context: PublicationCommandContext::try_new(
            receipt_identity,
            command_digest,
            expected_revision,
            occurred_at_millis,
        )?,
        command: DomainCancelCommand::try_new(
            command.payload.publication_id.clone(),
            command.payload.reason.clone(),
        )?,
    })
}

fn validate_query_identity(
    actor: &Actor,
    scope: &RepositoryScope,
    request_id: &winwincode_domain::RequestId,
    limit: i64,
) -> Result<usize, PublicationCommandError> {
    let limit = usize::try_from(limit)
        .ok()
        .filter(|limit| (1..=MAX_PAGE_SIZE).contains(limit))
        .ok_or_else(|| {
            PublicationCommandError::InvalidInput(
                "publication query page limit is invalid".to_owned(),
            )
        })?;
    command_receipt_identity(
        actor,
        &Scope::RepositoryScope(scope.clone()),
        request_id.clone(),
    )
    .map_err(|error| PublicationCommandError::InvalidInput(error.to_string()))?;
    repository_scope(scope)?;
    Ok(limit)
}

fn repository_scope(
    scope: &RepositoryScope,
) -> Result<RepositoryPolicyScope, PublicationCommandError> {
    RepositoryPolicyScope::try_new(
        scope.organization_id.clone(),
        scope.workspace_id.clone(),
        scope.project_id.clone(),
        scope.repository_id.clone(),
    )
    .map_err(PublicationCommandError::InvalidInput)
}

fn ensure_scope(
    scope: &RepositoryScope,
    publication: &Publication,
) -> Result<(), PublicationCommandError> {
    if publication.repository_scope_sha256() != &repository_scope(scope)?.sha256() {
        return Err(PublicationCommandError::InvalidInput(
            "publication is outside the requested repository scope".to_owned(),
        ));
    }
    Ok(())
}

fn normalized_states(states: &[String]) -> Result<Vec<String>, PublicationCommandError> {
    if states.iter().any(|state| {
        !matches!(
            state.as_str(),
            "pending" | "publishing" | "published" | "failed" | "cancelled"
        )
    }) {
        return Err(PublicationCommandError::InvalidInput(
            "publication list state filter is invalid".to_owned(),
        ));
    }
    let mut normalized = states.to_vec();
    normalized.sort_unstable();
    normalized.dedup();
    Ok(normalized)
}

#[allow(clippy::too_many_arguments)]
fn load_publication_page(
    storage: &mut dyn ProductStateStorage,
    scope: &RepositoryScope,
    delivery_id: Option<&winwincode_domain::DeliveryId>,
    states: &[String],
    after: Option<&PublicationId>,
    upper_bound: &str,
    limit: usize,
) -> Result<LoadedPublicationPage, PublicationCommandError> {
    validate_stream_bound(upper_bound)?;
    let expected_scope_sha256 = repository_scope(scope)?.sha256();
    let mut stream_after = String::new();
    let mut scanned = 0_usize;
    let mut items = Vec::with_capacity(limit.saturating_add(1));
    let mut after_seen = after.is_none();
    let mut snapshot = Sha256::new();
    loop {
        let rows = storage
            .scan_state_streams(
                PUBLICATION_STREAM_PREFIX,
                &stream_after,
                upper_bound,
                SCAN_PAGE_SIZE,
            )
            .map_err(publication_storage_error)?;
        if rows.is_empty() {
            break;
        }
        scanned = scanned.saturating_add(rows.len());
        if scanned > MAX_SNAPSHOT_ROWS {
            return Err(publication_storage_error(StorageError::adapter(
                "publication list snapshot exceeds its bounded scan budget",
            )));
        }
        for stored in &rows {
            let publication_id = publication_id_from_stream(&stored.stream_id)?;
            let publication = load_publication(storage, &publication_id)?;
            if publication.revision() != stored.revision {
                return Err(publication_storage_error(StorageError::adapter(
                    "publication list state revision does not match its journal",
                )));
            }
            if publication.repository_scope_sha256() != &expected_scope_sha256
                || delivery_id
                    .is_some_and(|delivery_id| publication.binding().delivery_id() != delivery_id)
                || !states.is_empty()
                    && !states
                        .iter()
                        .any(|state| state == publication.state().as_str())
            {
                continue;
            }
            let projection = publication_projection(&publication)?;
            let encoded = serde_json::to_vec(&projection).map_err(|_| {
                publication_storage_error(StorageError::adapter(
                    "publication projection cannot be encoded",
                ))
            })?;
            snapshot.update((encoded.len() as u64).to_be_bytes());
            snapshot.update(&encoded);
            if after.is_some_and(|after| after == &publication_id) {
                after_seen = true;
            }
            if after.is_none_or(|after| publication_id.0.as_str() > after.0.as_str())
                && items.len() <= limit
            {
                items.push(projection);
            }
        }
        stream_after.clone_from(
            &rows
                .last()
                .expect("a non-empty scan page has a final stream")
                .stream_id,
        );
        if rows.len() < SCAN_PAGE_SIZE || stream_after == upper_bound {
            break;
        }
    }
    let has_more = items.len() > limit;
    if has_more {
        items.pop();
    }
    Ok(LoadedPublicationPage {
        items,
        has_more,
        snapshot_sha256: format!("sha256:{:x}", snapshot.finalize()),
        after_seen,
    })
}

fn publication_id_from_stream(stream_id: &str) -> Result<PublicationId, PublicationCommandError> {
    let publication_id = stream_id
        .strip_prefix(PUBLICATION_STREAM_PREFIX)
        .filter(|value| !value.contains(':'))
        .ok_or_else(|| {
            publication_storage_error(StorageError::adapter(
                "publication stream identity is invalid",
            ))
        })?;
    let publication_id = PublicationId(publication_id.to_owned());
    validate_publication_id(&publication_id)?;
    Ok(publication_id)
}

fn validate_stream_bound(stream_id: &str) -> Result<(), PublicationCommandError> {
    publication_id_from_stream(stream_id).map(|_| ())
}

fn validate_publication_id(publication_id: &PublicationId) -> Result<(), PublicationCommandError> {
    let suffix = publication_id.0.strip_prefix("pub_").ok_or_else(|| {
        PublicationCommandError::InvalidInput("publication identity is invalid".to_owned())
    })?;
    if suffix.len() != 26
        || !suffix.bytes().all(|byte| {
            matches!(
                byte,
                b'0'..=b'9'
                    | b'A'..=b'H'
                    | b'J'
                    | b'K'
                    | b'M'
                    | b'N'
                    | b'P'..=b'T'
                    | b'V'..=b'Z'
            )
        })
    {
        return Err(PublicationCommandError::InvalidInput(
            "publication identity is invalid".to_owned(),
        ));
    }
    Ok(())
}

fn load_publication(
    storage: &mut dyn ProductStateStorage,
    publication_id: &PublicationId,
) -> Result<Publication, PublicationCommandError> {
    validate_publication_id(publication_id)?;
    let mut port = SideEffectBlockedPublicationPort;
    let coordinator = PublicationCoordinator::new(
        PublicationLedger::new(storage),
        &mut port,
        Box::new(UnavailablePublicationAudit),
    );
    coordinator
        .get(publication_id)
        .map_err(PublicationCommandError::from)
}

fn cancel_publication(
    storage: &mut dyn ProductStateStorage,
    context: &PublicationCommandContext,
    command: &DomainCancelCommand,
) -> Result<Publication, PublicationCommandError> {
    let mut port = SideEffectBlockedPublicationPort;
    PublicationCoordinator::new(
        PublicationLedger::new(storage),
        &mut port,
        Box::new(UnavailablePublicationAudit),
    )
    .cancel(context, command)
    .map_err(PublicationCommandError::from)
}

struct SideEffectBlockedPublicationPort;

impl PublicationPort for SideEffectBlockedPublicationPort {
    fn lookup(
        &mut self,
        _operation: &PublicationOperation,
    ) -> Result<PublicationPortObservation, PublicationPortError> {
        Err(
            PublicationPortError::new("publication-query-side-effect-blocked")
                .expect("the static Publication port error is canonical"),
        )
    }

    fn apply(
        &mut self,
        _operation: &PublicationOperation,
    ) -> Result<PublicationPortMutation, PublicationPortError> {
        Err(
            PublicationPortError::new("publication-query-side-effect-blocked")
                .expect("the static Publication port error is canonical"),
        )
    }
}

struct UnavailablePublicationAudit;

impl PublicationPolicyAudit for UnavailablePublicationAudit {
    fn record(
        &mut self,
        _decision: &winwincode_publication::PublicationPolicyDecision,
    ) -> Result<(), PublicationPolicyAuditError> {
        Err(PublicationPolicyAuditError::unavailable())
    }
}

pub(crate) fn publication_projection(
    publication: &Publication,
) -> Result<PublicationProjection, PublicationCommandError> {
    let fact = publication.result_fact()?;
    let binding = fact.binding();
    let approved_at =
        instant_from_millis(publication.approved_at_millis()).map_err(publication_storage_error)?;
    let target = publication.target();
    if target.provider() != "github" || target.kind() != "pull-request" {
        return Err(publication_storage_error(StorageError::adapter(
            "publication target is outside the generated contract",
        )));
    }
    let resource_ref = fact
        .resource()
        .map(|resource| {
            if resource.repository() != target.repository() {
                return Err(publication_storage_error(StorageError::adapter(
                    "publication resource is outside its target",
                )));
            }
            Ok(PublicationResourceRef {
                kind: match resource.kind() {
                    PublicationResourceKind::GitHubIssue => ApiPublicationResourceKind::GithubIssue,
                    PublicationResourceKind::GitHubPullRequest => {
                        ApiPublicationResourceKind::GithubPullRequest
                    }
                },
                number: i64::try_from(resource.number()).map_err(|_| {
                    publication_storage_error(StorageError::adapter(
                        "publication resource number exceeds the public range",
                    ))
                })?,
                repository: GitHubRepositorySlug(resource.repository().to_owned()),
            })
        })
        .transpose()?;
    Ok(PublicationProjection {
        approval_attention_item_id: binding.approval_id().clone(),
        approved_at,
        approved_by: ActorId::UserId(UserId(publication.approved_by().to_owned())),
        candidate_ref: binding.candidate_ref().to_owned(),
        delivery_id: binding.delivery_id().clone(),
        delivery_spec_id: binding.delivery_spec_id().to_owned(),
        delivery_spec_revision: revision(binding.delivery_spec_revision())?,
        delivery_verdict_id: binding.verdict_id().to_owned(),
        id: fact.publication_id().clone(),
        publication_set_sha256: fact.publication_set_sha256().clone(),
        resource_ref,
        revision: fact.revision().clone(),
        state: fact.state().to_owned(),
        target: ApiPublicationTarget {
            base_branch: target.base_branch().to_owned(),
            head_branch: target.head_branch().to_owned(),
            head_repository: GitHubRepositorySlug(target.head_repository().to_owned()),
            provider: PublicationTargetProvider::Github,
            repository: GitHubRepositorySlug(target.repository().to_owned()),
        },
        updated_at: fact.updated_at().clone(),
        verdict_status: PublicationProjectionVerdictStatus::Pass,
    })
}

#[derive(Clone, Copy)]
enum CancelAuditPhase {
    Requested,
    Completed,
}

impl CancelAuditPhase {
    const fn key(self) -> &'static [u8] {
        match self {
            Self::Requested => b"requested",
            Self::Completed => b"completed",
        }
    }

    const fn result_code(self) -> &'static str {
        match self {
            Self::Requested => "publication.cancel-requested",
            Self::Completed => "publication.cancelled",
        }
    }
}

#[allow(clippy::too_many_arguments)]
fn append_cancel_audit(
    store: &mut winwincode_audit::AuditStore,
    command: &PublicationCancelCommand,
    command_digest: &Sha256Digest,
    publication: &Publication,
    origin: &PublicationPolicyOrigin,
    occurred_at_millis: u64,
    phase: CancelAuditPhase,
) -> Result<(), PublicationCommandError> {
    let event = AuditEvent::succeeded(
        cancel_audit_event_id(command_digest, phase)?,
        occurred_at_millis,
        audit_actor(&command.actor),
        audit_scope(&command.scope)?,
        command.request_id.clone(),
        AuditAction::publication("publication.cancel")?,
        AuditState::unchanged(None)?,
        audit_origin(origin)?,
        AuditSubject::new()
            .with_delivery(publication.binding().delivery_id().clone())
            .with_publication(publication.id().clone()),
        phase.result_code(),
        AuditRetention::Indefinite,
    )?;
    store
        .append(&event)
        .map(|_| ())
        .map_err(PublicationCommandError::AuditUnavailable)
}

fn cancel_audit_occurred_at(
    store: &winwincode_audit::AuditStore,
    command: &PublicationCancelCommand,
    command_digest: &Sha256Digest,
    requested_at_millis: u64,
) -> Result<u64, PublicationCommandError> {
    let event_id = cancel_audit_event_id(command_digest, CancelAuditPhase::Requested)?;
    let access = audit_scope(&command.scope)?.into_access();
    let stored = store
        .read_exact(&access, &event_id, i64::MAX as u64)
        .map_err(PublicationCommandError::AuditUnavailable)?;
    Ok(stored
        .as_ref()
        .and_then(winwincode_audit::AuditRecord::event)
        .map_or(requested_at_millis, AuditEvent::occurred_at_millis))
}

fn cancel_audit_event_id(
    command_digest: &Sha256Digest,
    phase: CancelAuditPhase,
) -> Result<AuditEventId, PublicationCommandError> {
    let event_identity = digest_bytes(
        [
            b"winwincode.publication-cancel-audit.v1".as_slice(),
            &[0],
            command_digest.0.as_bytes(),
            &[0],
            phase.key(),
        ]
        .concat()
        .as_slice(),
    );
    AuditEventId::from_digest(&Sha256Digest(event_identity)).map_err(Into::into)
}

fn audit_actor(actor: &Actor) -> AuditActor {
    match actor {
        Actor::UserActor(actor) => AuditActor::User(actor.id.clone()),
        Actor::ServiceAccountActor(actor) => AuditActor::ServiceAccount(actor.id.clone()),
        Actor::SystemActor(actor) => AuditActor::System(actor.id.clone()),
    }
}

fn audit_scope(scope: &RepositoryScope) -> Result<AuditScope, winwincode_audit::AuditError> {
    AuditScope::repository(
        scope.organization_id.clone(),
        scope.workspace_id.clone(),
        scope.project_id.clone(),
        scope.repository_id.clone(),
    )
}

fn audit_origin(
    origin: &PublicationPolicyOrigin,
) -> Result<AuditOrigin, winwincode_audit::AuditError> {
    match origin {
        PublicationPolicyOrigin::Local { component } => AuditOrigin::local(component),
        PublicationPolicyOrigin::Network { source_ip } => Ok(AuditOrigin::network(*source_ip)),
    }
}

fn encode_cursor(cursor: &PublicationListCursor) -> Result<OpaqueCursor, PublicationCommandError> {
    serde_json::to_vec(cursor)
        .map(|bytes| OpaqueCursor(URL_SAFE_NO_PAD.encode(bytes)))
        .map_err(|_| {
            PublicationCommandError::InvalidInput(
                "publication list cursor cannot be encoded".to_owned(),
            )
        })
}

fn decode_cursor(
    cursor: Option<&OpaqueCursor>,
    scope_sha256: &str,
    filter_sha256: &str,
) -> Result<Option<PublicationListCursor>, PublicationCommandError> {
    let Some(cursor) = cursor else {
        return Ok(None);
    };
    if cursor.0.len() > MAX_CURSOR_BYTES {
        return Err(PublicationCommandError::ReadCursorExpired);
    }
    let bytes = URL_SAFE_NO_PAD
        .decode(cursor.0.as_bytes())
        .map_err(|_| PublicationCommandError::ReadCursorExpired)?;
    let decoded: PublicationListCursor =
        serde_json::from_slice(&bytes).map_err(|_| PublicationCommandError::ReadCursorExpired)?;
    if decoded.schema != CURSOR_SCHEMA
        || decoded.scope_sha256 != scope_sha256
        || decoded.filter_sha256 != filter_sha256
        || validate_stream_bound(&decoded.upper_bound_stream_id).is_err()
        || validate_publication_id(&decoded.after_publication_id).is_err()
        || decoded.after_publication_id.0.as_str()
            > decoded
                .upper_bound_stream_id
                .trim_start_matches(PUBLICATION_STREAM_PREFIX)
    {
        return Err(PublicationCommandError::ReadCursorExpired);
    }
    Ok(Some(decoded))
}

pub(crate) fn checked_response<T: Serialize>(value: T) -> Result<T, PublicationCommandError> {
    CredentialLeakGate::default()
        .inspect_serializable(CredentialOutputBoundary::Http, &value)
        .map_err(|_| {
            publication_storage_error(StorageError::adapter(
                "publication response failed the Credential output boundary",
            ))
        })?;
    Ok(value)
}

fn digest_json<T: Serialize + ?Sized>(value: &T) -> Result<String, PublicationCommandError> {
    serde_json::to_vec(value)
        .map(|bytes| digest_bytes(&bytes))
        .map_err(|_| {
            PublicationCommandError::InvalidInput(
                "publication list filter cannot be encoded".to_owned(),
            )
        })
}

fn digest_bytes(bytes: &[u8]) -> String {
    format!("sha256:{:x}", Sha256::digest(bytes))
}

fn revision(value: u64) -> Result<Revision, PublicationCommandError> {
    i64::try_from(value)
        .map(Revision)
        .map_err(|_| PublicationCommandError::InvalidInput("revision is invalid".to_owned()))
}

fn previous_revision(value: u64) -> Result<Revision, PublicationCommandError> {
    value
        .checked_sub(1)
        .ok_or_else(|| PublicationCommandError::InvalidInput("revision is invalid".to_owned()))
        .and_then(revision)
}

fn publication_storage_error(error: StorageError) -> PublicationCommandError {
    PublicationCommandError::from(winwincode_publication::PublicationError::from(error))
}
