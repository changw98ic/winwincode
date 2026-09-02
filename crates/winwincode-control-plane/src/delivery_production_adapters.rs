// SPDX-License-Identifier: Apache-2.0

//! Production-local Delivery authority and durable scheduler dispatch.
//!
//! The adapter resolves repository facts from one configured Git checkout and
//! offers immutable jobs to the canonical `SQLite` execution queue. It never
//! invents Worker, lease, terminal-outcome, candidate, or verdict authority;
//! stage handoff and verdict submission remain closed until those exact
//! durable facts are available.

use std::{
    fmt,
    path::{Path, PathBuf},
    time::{SystemTime, UNIX_EPOCH},
};

use serde::Serialize;
use sha2::{Digest, Sha256};
use winwincode_api::generated::{
    Actor, CommandName, DeliveryAdvancePayload, DeliveryCreatePayload,
    DeliveryResolveAttentionPayload, DeliveryUpdateSpecPayload, RepositoryScope, Scope,
};
use winwincode_delivery::{
    application::{
        attention::{AttentionDecision, ResolveAttentionInput, resolve_attention},
        stage::{
            AdvanceStageInput, NewStageIdentities, ReviewAttentionSeed, StageAdvanceEffect,
            StageAdvanceResult, advance,
        },
    },
    domain::{
        AttentionItemStatus, DELIVERY_SCHEMA_VERSION, Delivery, DeliveryStage, DeliveryStatus,
        RepositoryKind, RepositoryRef, SessionBindingId, StageRunStatus,
    },
};
use winwincode_domain::{
    AttentionItemId, ExecutionJobId, ProductSessionId, RequestId, Sha256Digest, StageRunId,
};
use winwincode_execution_port::generated::{
    ExecutionJob, ExecutionLimits, ExecutionScope, ExecutionWorkspace, ExecutionWorkspaceWriteMode,
};
use winwincode_repository_context::{
    CommandPurpose, RepositoryContext, RepositoryContextPort, RepositoryContextQuery,
    RepositoryContextScanner,
};
use winwincode_storage::{
    ArtifactStore, ExecutionJobSubmission, ExecutionQueueScope, LocalArtifactObjectStore,
    LocalGitSourceResolver, SqliteStorage, StorageError,
};

use crate::{
    ControlPlane, ControlPlaneConfig, DeliveryAdvanceAuthority, DeliveryAttentionAuthority,
    DeliveryAuthorityError, DeliveryAuthorityPort, DeliveryAuthorityRequest,
    DeliverySpecificationAuthority, DeliveryVerdictAuthority, EventPublisher, StartError,
    delivery_execution::{
        DeliveryExecutionConfig, DeliveryExecutionPortError, ExecutionJobDispatcher,
    },
};

const DEFAULT_MAX_REWORK_ATTEMPTS: u64 = 3;
const DEFAULT_MAX_RUNTIME_SECONDS: i64 = 3_600;
const DEFAULT_MAX_ARTIFACT_BYTES: i64 = 1_073_741_824;
const MAX_SAFE_INTEGER: u64 = 9_007_199_254_740_991;
const CROCKFORD_BASE32: &[u8; 32] = b"0123456789ABCDEFGHJKMNPQRSTVWXYZ";

/// Exact local repository and tenant scope installed at process startup.
#[derive(Clone, Debug, PartialEq)]
pub struct LocalDeliveryAdapterConfig {
    repository_root: PathBuf,
    repository_scope: RepositoryScope,
    max_rework_attempts: u64,
    max_runtime_seconds: i64,
    max_artifact_bytes: i64,
}

impl LocalDeliveryAdapterConfig {
    #[must_use]
    pub fn new(repository_root: impl AsRef<Path>, repository_scope: RepositoryScope) -> Self {
        Self {
            repository_root: repository_root.as_ref().to_path_buf(),
            repository_scope,
            max_rework_attempts: DEFAULT_MAX_REWORK_ATTEMPTS,
            max_runtime_seconds: DEFAULT_MAX_RUNTIME_SECONDS,
            max_artifact_bytes: DEFAULT_MAX_ARTIFACT_BYTES,
        }
    }

    /// Sets the bounded runtime policy applied to every generated job.
    #[must_use]
    pub const fn with_execution_limits(
        mut self,
        max_runtime_seconds: i64,
        max_artifact_bytes: i64,
    ) -> Self {
        self.max_runtime_seconds = max_runtime_seconds;
        self.max_artifact_bytes = max_artifact_bytes;
        self
    }

    /// Sets the repository-local rework ceiling.
    #[must_use]
    pub const fn with_max_rework_attempts(mut self, max_rework_attempts: u64) -> Self {
        self.max_rework_attempts = max_rework_attempts;
        self
    }

    #[must_use]
    pub fn repository_root(&self) -> &Path {
        &self.repository_root
    }

    #[must_use]
    pub const fn repository_scope(&self) -> &RepositoryScope {
        &self.repository_scope
    }
}

/// Startup failure for the production-local Delivery adapters.
#[derive(Debug)]
pub struct LocalDeliveryAdapterError {
    message: String,
}

impl LocalDeliveryAdapterError {
    fn new(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
        }
    }
}

impl fmt::Display for LocalDeliveryAdapterError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.message)
    }
}

impl std::error::Error for LocalDeliveryAdapterError {}

/// Repository-backed production authority. Missing scheduler/runtime facts
/// are reported as closed authority rather than synthesized defaults.
pub struct LocalDeliveryAuthority {
    config: LocalDeliveryAdapterConfig,
    repository_root: PathBuf,
    repository_source_root: PathBuf,
    repository_locator: String,
    scanner: RepositoryContextScanner,
    storage: SqliteStorage,
    artifacts: ArtifactStore,
    source_resolver: LocalGitSourceResolver,
}

impl LocalDeliveryAuthority {
    /// Opens one exact canonical Git checkout as the repository authority.
    ///
    /// # Errors
    ///
    /// Rejects a missing repository, invalid limits, or a non-canonical scope.
    pub fn open(
        config: LocalDeliveryAdapterConfig,
        data_directory: impl AsRef<Path>,
    ) -> Result<Self, LocalDeliveryAdapterError> {
        let data_directory = data_directory.as_ref();
        validate_config(&config)?;
        let repository_root = std::fs::canonicalize(&config.repository_root).map_err(|error| {
            LocalDeliveryAdapterError::new(format!(
                "failed to resolve the configured Delivery repository: {error}"
            ))
        })?;
        if !repository_root.join(".git").exists() {
            return Err(LocalDeliveryAdapterError::new(
                "configured Delivery repository is not a Git worktree",
            ));
        }
        let repository_source_root =
            repository_root
                .parent()
                .map(Path::to_path_buf)
                .ok_or_else(|| {
                    LocalDeliveryAdapterError::new(
                        "configured Delivery repository has no controlled parent",
                    )
                })?;
        let repository_locator = portable_repository_locator(&repository_root)?;
        let storage = SqliteStorage::open(data_directory).map_err(|error| {
            LocalDeliveryAdapterError::new(format!(
                "failed to open Delivery authority storage: {error}"
            ))
        })?;
        let objects =
            LocalArtifactObjectStore::open(data_directory.join("artifacts")).map_err(|error| {
                LocalDeliveryAdapterError::new(format!(
                    "failed to open Delivery Artifact object storage: {error}"
                ))
            })?;
        let artifacts =
            ArtifactStore::open(data_directory.join("artifact-catalog"), Box::new(objects))
                .map_err(|error| {
                    LocalDeliveryAdapterError::new(format!(
                        "failed to open Delivery Artifact catalog: {error}"
                    ))
                })?;
        let source_resolver =
            LocalGitSourceResolver::open(&repository_source_root).map_err(|error| {
                LocalDeliveryAdapterError::new(format!(
                    "failed to open Delivery Git source authority: {error}"
                ))
            })?;
        Ok(Self {
            config,
            repository_root,
            repository_source_root,
            repository_locator,
            scanner: RepositoryContextScanner::default(),
            storage,
            artifacts,
            source_resolver,
        })
    }

    fn now_millis(delivery: Option<&Delivery>) -> Result<u64, DeliveryAuthorityError> {
        let system = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_err(|_| DeliveryAuthorityError::new("system clock precedes the Unix epoch"))?
            .as_millis();
        let system = u64::try_from(system)
            .map_err(|_| DeliveryAuthorityError::new("system clock exceeds the durable range"))?;
        bounded_authority_time(
            system,
            delivery.map(|current| current.snapshot().updated_at_millis),
        )
    }

    fn require_scope(
        &self,
        request: &DeliveryAuthorityRequest<'_>,
    ) -> Result<(), DeliveryAuthorityError> {
        let Scope::RepositoryScope(scope) = &request.command().scope else {
            return Err(DeliveryAuthorityError::new(
                "Delivery authority requires repository scope",
            ));
        };
        if scope != &self.config.repository_scope {
            return Err(DeliveryAuthorityError::new(
                "Delivery command is outside the configured repository scope",
            ));
        }
        Ok(())
    }

    fn inspect(&self, baseline: &str) -> Result<RepositoryContext, DeliveryAuthorityError> {
        let context = self
            .scanner
            .inspect(&RepositoryContextQuery::new(
                &self.repository_root,
                baseline,
            ))
            .map_err(|error| {
                DeliveryAuthorityError::new(format!(
                    "repository baseline authority is unavailable: {error}"
                ))
            })?;
        if !context.baseline_verified || context.baseline_sha != baseline {
            return Err(DeliveryAuthorityError::new(
                "repository baseline authority returned stale facts",
            ));
        }
        Ok(context)
    }

    fn repository_ref(&self) -> RepositoryRef {
        RepositoryRef {
            schema_version: DELIVERY_SCHEMA_VERSION,
            kind: RepositoryKind::LocalGit,
            locator: self.repository_locator.clone(),
        }
    }

    fn terminal_authority(
        &mut self,
        delivery: &Delivery,
        command: &winwincode_api::generated::CommandEnvelope,
    ) -> Result<
        Option<crate::terminal_outcome_transaction::DeliveryTerminalHandoff>,
        DeliveryAuthorityError,
    > {
        let active = delivery.snapshot().stage_runs.iter().any(|run| {
            matches!(
                run.status,
                StageRunStatus::Running | StageRunStatus::Waiting
            )
        });
        if !active {
            return Ok(None);
        }
        crate::terminal_outcome_transaction::load_active_terminal_handoff(
            &mut self.storage,
            delivery,
            command,
        )
        .map_err(|error| storage_authority_error(&error))?
        .map_or_else(
            || {
                Err(DeliveryAuthorityError::new(
                    "active Delivery stage has no durable terminal authority",
                ))
            },
            |facts| Ok(Some(facts)),
        )
    }

    fn execution_config(
        &self,
        delivery: &Delivery,
        transition: &StageAdvanceResult,
        terminal_handoff: Option<&crate::terminal_outcome_transaction::DeliveryTerminalHandoff>,
        now_millis: u64,
    ) -> Result<Option<DeliveryExecutionConfig>, DeliveryAuthorityError> {
        let StageAdvanceEffect::Dispatch(intent) = &transition.effect else {
            return Ok(None);
        };
        let deadline_millis = now_millis
            .checked_add(
                u64::try_from(self.config.max_runtime_seconds)
                    .unwrap_or_default()
                    .saturating_mul(1_000),
            )
            .filter(|value| *value <= MAX_SAFE_INTEGER)
            .ok_or_else(|| {
                DeliveryAuthorityError::new("Delivery execution deadline exceeds the range")
            })?;
        let payload = serde_json::to_vec(&(
            &self.config.repository_scope,
            delivery.id(),
            delivery.revision(),
            &transition.delivery,
        ))
        .map_err(|error| DeliveryAuthorityError::new(error.to_string()))?;
        let (candidate_ref, checkout_revision) = match &transition.effect {
            StageAdvanceEffect::Dispatch(intent) if intent.stage == DeliveryStage::Verifying => {
                let pending_executor =
                    terminal_handoff.filter(|handoff| pending_executor_handoff(delivery, handoff));
                let candidate = if let Some(handoff) = pending_executor {
                    crate::delivery_verdict_authority::resolve_pending_executor_candidate(
                        &self.artifacts,
                        &self.source_resolver,
                        &self.config.repository_scope,
                        &transition.delivery,
                        handoff.facts(),
                    )?
                } else {
                    crate::delivery_verdict_authority::resolve_current_candidate(
                        &self.storage,
                        &self.artifacts,
                        &self.source_resolver,
                        &self.config.repository_scope,
                        &transition.delivery,
                    )?
                    .ok_or_else(|| {
                        DeliveryAuthorityError::new(
                            "verification dispatch has no exact frozen writer candidate",
                        )
                    })?
                };
                (
                    Some(candidate.candidate_ref().to_owned()),
                    candidate.candidate_commit_id().to_owned(),
                )
            }
            StageAdvanceEffect::Dispatch(intent) if intent.stage == DeliveryStage::Reworking => {
                let authorization = intent.rework_authorization().ok_or_else(|| {
                    DeliveryAuthorityError::new(
                        "rework dispatch has no exact sealed candidate authorization",
                    )
                })?;
                (
                    None,
                    authorization
                        .previous_candidate()
                        .candidate_commit_id()
                        .to_owned(),
                )
            }
            _ => (None, delivery.snapshot().spec.base_revision.clone()),
        };
        Ok(Some(DeliveryExecutionConfig {
            payload_digest: Sha256Digest(format!("sha256:{:x}", Sha256::digest(payload))),
            candidate_ref,
            workspace: ExecutionWorkspace {
                checkout_revision,
                repository_id: self.config.repository_scope.repository_id.clone(),
                write_mode: if matches!(intent.role.as_str(), "executor" | "remediator") {
                    ExecutionWorkspaceWriteMode::Candidate
                } else {
                    ExecutionWorkspaceWriteMode::ReadOnly
                },
            },
            limits: ExecutionLimits {
                deadline_at: crate::instant_from_millis(deadline_millis)
                    .map_err(|error| storage_authority_error(&error))?,
                max_artifact_bytes: self.config.max_artifact_bytes,
                max_runtime_seconds: self.config.max_runtime_seconds,
            },
        }))
    }
}

fn pending_executor_handoff(
    delivery: &Delivery,
    handoff: &crate::terminal_outcome_transaction::DeliveryTerminalHandoff,
) -> bool {
    delivery.snapshot().stage_runs.iter().any(|run| {
        run.id == *handoff.facts().stage_run_id()
            && run.stage == DeliveryStage::Executing
            && run.role == "executor"
            && matches!(
                run.status,
                StageRunStatus::Running | StageRunStatus::Waiting
            )
    })
}

impl DeliveryAuthorityPort for LocalDeliveryAuthority {
    fn specification(
        &mut self,
        request: DeliveryAuthorityRequest<'_>,
    ) -> Result<DeliverySpecificationAuthority, DeliveryAuthorityError> {
        self.require_scope(&request)?;
        let (baseline, repository_id, criteria) = match request.command().command {
            CommandName::DeliveryCreate => {
                let payload: DeliveryCreatePayload = decode_payload(request.command())?;
                (
                    payload.spec.base_revision,
                    payload.spec.repository_id,
                    payload.spec.acceptance_criteria,
                )
            }
            CommandName::DeliveryUpdateSpec => {
                let payload: DeliveryUpdateSpecPayload = decode_payload(request.command())?;
                (
                    payload.spec.base_revision,
                    payload.spec.repository_id,
                    payload.spec.acceptance_criteria,
                )
            }
            _ => {
                return Err(DeliveryAuthorityError::new(
                    "specification authority received another command",
                ));
            }
        };
        if repository_id != self.config.repository_scope.repository_id {
            return Err(DeliveryAuthorityError::new(
                "Delivery specification names another repository",
            ));
        }
        let context = self.inspect(&baseline)?;
        let verification = select_verification_command(&context)?;
        Ok(DeliverySpecificationAuthority {
            now_millis: Self::now_millis(request.delivery())?,
            repository: self.repository_ref(),
            source_ref: request
                .delivery()
                .and_then(|delivery| delivery.snapshot().spec.source_ref.clone()),
            max_rework_attempts: self.config.max_rework_attempts,
            criterion_verification_methods: criteria
                .into_iter()
                .map(|criterion| (criterion.id, verification.clone()))
                .collect(),
        })
    }

    fn advance(
        &mut self,
        request: DeliveryAuthorityRequest<'_>,
    ) -> Result<DeliveryAdvanceAuthority, DeliveryAuthorityError> {
        self.require_scope(&request)?;
        if request.command().command != CommandName::DeliveryAdvance {
            return Err(DeliveryAuthorityError::new(
                "advance authority received another command",
            ));
        }
        let payload: DeliveryAdvancePayload = decode_payload(request.command())?;
        let delivery = request
            .delivery()
            .ok_or_else(|| DeliveryAuthorityError::new("current Delivery is missing"))?;
        if payload.delivery_id != *delivery.id()
            || delivery.snapshot().spec.repository.locator != self.repository_locator
        {
            return Err(DeliveryAuthorityError::new(
                "Delivery advance does not match the configured repository",
            ));
        }
        let active_stage = delivery.snapshot().stage_runs.iter().find(|run| {
            matches!(
                run.status,
                StageRunStatus::Running | StageRunStatus::Waiting
            )
        });
        let terminal_handoff = self.terminal_authority(delivery, request.command())?;
        let previous_outcome = terminal_handoff
            .as_ref()
            .map(|handoff| handoff.facts().verify_active(delivery))
            .transpose()
            .map_err(|error| DeliveryAuthorityError::new(error.to_string()))?;
        let current_lease = terminal_handoff
            .as_ref()
            .map(|handoff| handoff.facts().authority().active_lease().clone());
        let seed = command_seed(request.command())?;
        let actor = actor_id(&request.command().actor);
        let review_handoff = review_handoff(delivery, active_stage);
        let now_millis = if review_handoff == ReviewHandoff::Plan {
            previous_outcome.as_ref().map_or_else(
                || Self::now_millis(Some(delivery)),
                |outcome| Ok(outcome.finished_at_millis()),
            )?
        } else {
            Self::now_millis(Some(delivery))?
        };
        let review_title = format!("Review Delivery {}", delivery.id().0);
        let input = AdvanceStageInput {
            expected_revision: delivery.revision(),
            product_session_id: ProductSessionId(deterministic_id(
                "psn",
                seed_with(&seed, b"product-session"),
            )),
            identities: NewStageIdentities {
                stage_run_id: StageRunId(deterministic_id("run", seed_with(&seed, b"stage"))),
                execution_job_id: ExecutionJobId(deterministic_id("job", seed_with(&seed, b"job"))),
                session_binding_id: SessionBindingId(deterministic_id(
                    "binding",
                    seed_with(&seed, b"binding"),
                )),
                attention_item_id: AttentionItemId(deterministic_id(
                    "att",
                    seed_with(&seed, b"attention"),
                )),
            },
            review: (review_handoff == ReviewHandoff::Delivery).then(|| ReviewAttentionSeed {
                title: review_title.clone(),
                context: format!(
                    "delivery={} revision={} spec={}",
                    delivery.id().0,
                    delivery.revision(),
                    delivery.snapshot().spec.id.0
                ),
                assigned_to: actor,
            }),
            previous_outcome,
            current_lease,
            rework_authorization: None,
            now_millis,
        };
        let transition = if review_handoff == ReviewHandoff::Plan {
            crate::planning_solution_authority::prepare(
                &self.storage,
                &self.config.repository_scope,
                delivery,
                input,
                review_title,
                actor_id(&request.command().actor),
            )?
        } else {
            advance(delivery, input)
                .map_err(|error| DeliveryAuthorityError::new(error.to_string()))?
        };
        let execution =
            self.execution_config(delivery, &transition, terminal_handoff.as_ref(), now_millis)?;
        Ok(DeliveryAdvanceAuthority {
            repository: delivery.snapshot().spec.repository.clone(),
            source_ref: delivery.snapshot().spec.source_ref.clone(),
            transition,
            execution,
            terminal_handoff,
        })
    }

    fn resolve_attention(
        &mut self,
        request: DeliveryAuthorityRequest<'_>,
    ) -> Result<DeliveryAttentionAuthority, DeliveryAuthorityError> {
        self.require_scope(&request)?;
        if request.command().command != CommandName::DeliveryResolveAttention {
            return Err(DeliveryAuthorityError::new(
                "Attention authority received another command",
            ));
        }
        let payload: DeliveryResolveAttentionPayload = decode_payload(request.command())?;
        let delivery = request
            .delivery()
            .ok_or_else(|| DeliveryAuthorityError::new("current Delivery is missing"))?;
        if payload.delivery_id != *delivery.id() || payload.remediation.is_some() {
            return Err(DeliveryAuthorityError::new(
                "Attention request is foreign or requires the separate rework authority",
            ));
        }
        let mut items = delivery.snapshot().attention_items.iter().filter(|item| {
            item.id == payload.attention_item_id && item.status == AttentionItemStatus::Open
        });
        let item = items.next().ok_or_else(|| {
            DeliveryAuthorityError::new("current open Attention item does not exist")
        })?;
        if items.next().is_some() {
            return Err(DeliveryAuthorityError::new(
                "current Attention identity is ambiguous",
            ));
        }
        let stage_run_id = item.stage_run_id.clone().ok_or_else(|| {
            DeliveryAuthorityError::new("current Attention item has no StageRun identity")
        })?;
        let decision = match payload.decision.as_str() {
            "resolve" => AttentionDecision::Resolved,
            "dismiss" => AttentionDecision::Dismissed,
            _ => {
                return Err(DeliveryAuthorityError::new(
                    "Attention decision is not canonical",
                ));
            }
        };
        let transition = resolve_attention(
            delivery,
            ResolveAttentionInput {
                expected_revision: delivery.revision(),
                attention_item_id: payload.attention_item_id,
                stage_run_id,
                expected_context: item.context.clone(),
                actor: actor_id(&request.command().actor),
                decision,
                resolution: payload.resolution,
                now_millis: Self::now_millis(Some(delivery))?,
            },
        )
        .map_err(|error| DeliveryAuthorityError::new(error.to_string()))?;
        Ok(DeliveryAttentionAuthority {
            repository: delivery.snapshot().spec.repository.clone(),
            source_ref: delivery.snapshot().spec.source_ref.clone(),
            transition,
        })
    }

    fn verdict(
        &mut self,
        request: DeliveryAuthorityRequest<'_>,
    ) -> Result<DeliveryVerdictAuthority, DeliveryAuthorityError> {
        self.require_scope(&request)?;
        if request.command().command != CommandName::DeliverySubmitVerdict {
            return Err(DeliveryAuthorityError::new(
                "verdict authority received another command",
            ));
        }
        let delivery = request
            .delivery()
            .ok_or_else(|| DeliveryAuthorityError::new("current Delivery is missing"))?;
        if delivery.snapshot().spec.repository.locator != self.repository_locator {
            return Err(DeliveryAuthorityError::new(
                "Delivery verdict does not match the configured repository",
            ));
        }
        crate::delivery_verdict_authority::resolve(
            &self.storage,
            &self.artifacts,
            &self.source_resolver,
            &self.config.repository_scope,
            delivery,
        )
    }
}

/// Production dispatcher backed by the canonical scheduler execution queue.
pub struct LocalExecutionJobDispatcher {
    storage: SqliteStorage,
    repository_scope: RepositoryScope,
}

impl LocalExecutionJobDispatcher {
    /// Opens the canonical local database used by the Control Plane.
    ///
    /// # Errors
    ///
    /// Returns an error when `SQLite` or the queue schema cannot be opened.
    pub fn open(
        data_directory: impl AsRef<Path>,
        repository_scope: RepositoryScope,
    ) -> Result<Self, LocalDeliveryAdapterError> {
        let mut storage = SqliteStorage::open(data_directory).map_err(|error| {
            LocalDeliveryAdapterError::new(format!(
                "failed to open the Delivery execution queue: {error}"
            ))
        })?;
        storage.execution_queue().map_err(|error| {
            LocalDeliveryAdapterError::new(format!(
                "failed to migrate the Delivery execution queue: {error}"
            ))
        })?;
        Ok(Self {
            storage,
            repository_scope,
        })
    }
}

impl ExecutionJobDispatcher for LocalExecutionJobDispatcher {
    fn dispatch(&mut self, job: &ExecutionJob) -> Result<(), DeliveryExecutionPortError> {
        let ExecutionScope::DeliveryStageExecutionScope(job_scope) = &job.scope else {
            return Err(DeliveryExecutionPortError::new(
                "Delivery dispatcher received a foreign execution scope",
            ));
        };
        if job.workspace.repository_id != self.repository_scope.repository_id {
            return Err(DeliveryExecutionPortError::new(
                "Delivery job belongs to another repository",
            ));
        }
        let dispatch_payload = serde_json::to_vec(job).map_err(|error| {
            DeliveryExecutionPortError::new(format!(
                "failed to encode the immutable ExecutionJob: {error}"
            ))
        })?;
        let request_id = RequestId(deterministic_id(
            "req",
            [b"execution-queue:".as_slice(), job.job_id.0.as_bytes()].concat(),
        ));
        let scope = ExecutionQueueScope {
            organization_id: self.repository_scope.organization_id.clone(),
            workspace_id: self.repository_scope.workspace_id.clone(),
            project_id: self.repository_scope.project_id.clone(),
            repository_id: self.repository_scope.repository_id.clone(),
            product_session_id: job_scope.product_session_id.clone(),
            delivery_id: Some(job_scope.delivery_id.clone()),
        };
        let attempt = u64::try_from(job.attempt).map_err(|_| {
            DeliveryExecutionPortError::new("ExecutionJob attempt is outside the queue range")
        })?;
        let mut queue = self
            .storage
            .execution_queue()
            .map_err(|error| dispatch_error(&error))?;
        if let Some(existing) = queue
            .load_job(&scope, &job.job_id)
            .map_err(|error| dispatch_error(&error))?
        {
            let exact = existing.scope == scope
                && existing.job_id == job.job_id
                && existing.submission_request_id == request_id
                && existing.payload_digest == job.payload_digest
                && existing.dispatch_payload == dispatch_payload
                && existing.attempt == attempt
                && existing.dependencies.is_empty()
                && existing.stage_run_id == Some(job_scope.stage_run_id.clone());
            return if exact {
                Ok(())
            } else {
                Err(DeliveryExecutionPortError::new(
                    "durable execution queue contains another job body",
                ))
            };
        }
        let submitted_at = current_instant().map_err(|error| dispatch_error(&error))?;
        queue
            .submit(&ExecutionJobSubmission {
                scope,
                job_id: job.job_id.clone(),
                request_id,
                payload_digest: job.payload_digest.clone(),
                dispatch_payload,
                attempt,
                dependencies: Vec::new(),
                stage_run_id: Some(job_scope.stage_run_id.clone()),
                submitted_at,
            })
            .map_err(|error| dispatch_error(&error))?;
        Ok(())
    }
}

impl ControlPlane {
    /// Installs the repository authority and durable execution queue together.
    ///
    /// # Errors
    ///
    /// Rejects an injected-storage host, invalid configuration, or replacing
    /// either live adapter.
    pub fn install_local_delivery_adapters(
        &mut self,
        config: LocalDeliveryAdapterConfig,
    ) -> Result<(), LocalDeliveryAdapterError> {
        if self.delivery_authority.is_some()
            || self.delivery_dispatcher.is_some()
            || self.strongflow_sources.is_some()
            || self.git_source_resolver.is_some()
            || self.git_repository_root.is_some()
        {
            return Err(LocalDeliveryAdapterError::new(
                "Delivery production adapters are already installed",
            ));
        }
        let data_directory = self
            .local_database_path
            .as_deref()
            .and_then(Path::parent)
            .ok_or_else(|| {
                LocalDeliveryAdapterError::new(
                    "Delivery production adapters require local Control Plane storage",
                )
            })?;
        let repository_scope = config.repository_scope.clone();
        let authority = LocalDeliveryAuthority::open(config, data_directory)?;
        // Candidate source reconstruction and Git retention must share the
        // resolver's one canonical parent root.  Keep the root until every
        // adapter has opened successfully, then install both authorities as
        // one composition step below.
        let repository_source_root = authority.repository_source_root.clone();
        let dispatcher = LocalExecutionJobDispatcher::open(data_directory, repository_scope)?;
        let source_resolver =
            LocalGitSourceResolver::open(&repository_source_root).map_err(|error| {
                LocalDeliveryAdapterError::new(format!(
                    "failed to open the local Git candidate resolver: {error}"
                ))
            })?;
        self.install_strongflow_projection_sources(
            crate::strongflow_projection::production_sources(),
        )
        .map_err(|error| {
            LocalDeliveryAdapterError::new(format!(
                "failed to install StrongFlow projection sources: {error}"
            ))
        })?;
        self.delivery_authority = Some(Box::new(authority));
        self.delivery_dispatcher = Some(Box::new(dispatcher));
        self.git_repository_root = Some(repository_source_root);
        self.git_source_resolver = Some(Box::new(source_resolver));
        Ok(())
    }

    /// Starts local storage and installs the production Delivery adapters
    /// before exposing the running host.
    ///
    /// # Errors
    ///
    /// Closes the host and returns a startup error when adapter composition
    /// fails.
    pub fn start_local_with_delivery_adapters(
        config: ControlPlaneConfig,
        publisher: Box<dyn EventPublisher>,
        delivery: LocalDeliveryAdapterConfig,
    ) -> Result<Self, StartError> {
        let mut control_plane = Self::start_local(config, publisher)?;
        if let Err(error) = control_plane.install_local_delivery_adapters(delivery) {
            let cleanup = control_plane
                .shutdown()
                .err()
                .map_or_else(String::new, |source| {
                    format!("; cleanup also failed: {source}")
                });
            return Err(StartError::new(format!(
                "failed to install production Delivery adapters: {error}{cleanup}"
            )));
        }
        Ok(control_plane)
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum ReviewHandoff {
    None,
    Plan,
    Delivery,
}

fn review_handoff(
    delivery: &Delivery,
    active_stage: Option<&winwincode_delivery::domain::StageRun>,
) -> ReviewHandoff {
    match (
        delivery.snapshot().status,
        active_stage.map(|run| run.stage),
    ) {
        (DeliveryStatus::Planning, Some(_)) => ReviewHandoff::Plan,
        (DeliveryStatus::ReadyToDeliver, None) => ReviewHandoff::Delivery,
        _ => ReviewHandoff::None,
    }
}

fn validate_config(config: &LocalDeliveryAdapterConfig) -> Result<(), LocalDeliveryAdapterError> {
    if config.repository_root.as_os_str().is_empty()
        || !(0..=100).contains(&config.max_rework_attempts)
        || !(1..=604_800).contains(&config.max_runtime_seconds)
        || !(0..=1_099_511_627_776).contains(&config.max_artifact_bytes)
    {
        return Err(LocalDeliveryAdapterError::new(
            "Delivery adapter configuration is outside the supported bounds",
        ));
    }
    crate::repository_scope_key(&config.repository_scope)
        .map_err(|error| LocalDeliveryAdapterError::new(error.to_string()))?;
    Ok(())
}

fn portable_repository_locator(
    repository_root: &Path,
) -> Result<String, LocalDeliveryAdapterError> {
    let locator = repository_root
        .file_name()
        .and_then(std::ffi::OsStr::to_str)
        .filter(|value| {
            !value.is_empty()
                && !matches!(*value, "." | "..")
                && !value.contains(['/', '\\'])
                && !value.bytes().any(|byte| byte <= 31 || byte == 127)
        })
        .ok_or_else(|| {
            LocalDeliveryAdapterError::new(
                "configured Delivery repository has no portable relative locator",
            )
        })?;
    Ok(locator.to_owned())
}

fn bounded_authority_time(
    system_millis: u64,
    delivery_updated_at_millis: Option<u64>,
) -> Result<u64, DeliveryAuthorityError> {
    if system_millis > MAX_SAFE_INTEGER {
        return Err(DeliveryAuthorityError::new(
            "system clock exceeds the durable range",
        ));
    }
    let Some(updated_at_millis) = delivery_updated_at_millis else {
        return Ok(system_millis);
    };
    let next = updated_at_millis
        .checked_add(1)
        .filter(|value| *value <= MAX_SAFE_INTEGER)
        .ok_or_else(|| DeliveryAuthorityError::new("Delivery clock exceeds the durable range"))?;
    Ok(system_millis.max(next))
}

fn select_verification_command(
    context: &RepositoryContext,
) -> Result<String, DeliveryAuthorityError> {
    const PREFERENCE: [CommandPurpose; 5] = [
        CommandPurpose::Verify,
        CommandPurpose::Test,
        CommandPurpose::TypeCheck,
        CommandPurpose::Lint,
        CommandPurpose::StaticAnalysis,
    ];
    PREFERENCE
        .iter()
        .find_map(|purpose| {
            context
                .commands
                .iter()
                .find(|command| &command.purpose == purpose)
        })
        .map(|command| command.command.clone())
        .ok_or_else(|| {
            DeliveryAuthorityError::new(
                "repository baseline exposes no trusted verification command",
            )
        })
}

fn decode_payload<T: serde::de::DeserializeOwned + Serialize>(
    command: &winwincode_api::generated::CommandEnvelope,
) -> Result<T, DeliveryAuthorityError> {
    let payload: T = serde_json::from_value(command.payload.clone())
        .map_err(|error| DeliveryAuthorityError::new(format!("payload is invalid: {error}")))?;
    if serde_json::to_value(&payload)
        .map_err(|error| DeliveryAuthorityError::new(error.to_string()))?
        != command.payload
    {
        return Err(DeliveryAuthorityError::new("payload is not canonical"));
    }
    Ok(payload)
}

fn actor_id(actor: &Actor) -> String {
    match actor {
        Actor::UserActor(actor) => actor.id.0.clone(),
        Actor::ServiceAccountActor(actor) => actor.id.0.clone(),
        Actor::SystemActor(actor) => actor.id.0.clone(),
    }
}

fn command_seed(
    command: &winwincode_api::generated::CommandEnvelope,
) -> Result<Vec<u8>, DeliveryAuthorityError> {
    serde_json::to_vec(command)
        .map_err(|error| DeliveryAuthorityError::new(format!("command cannot be sealed: {error}")))
}

fn seed_with<'seed>(seed: &'seed [u8], label: &[u8]) -> impl AsRef<[u8]> + 'seed {
    [seed, label].concat()
}

fn deterministic_id(prefix: &str, seed: impl AsRef<[u8]>) -> String {
    let digest = Sha256::digest(seed.as_ref());
    let suffix = digest
        .iter()
        .take(26)
        .map(|byte| char::from(CROCKFORD_BASE32[usize::from(byte & 31)]))
        .collect::<String>();
    format!("{prefix}_{suffix}")
}

fn current_instant() -> Result<winwincode_domain::Instant, StorageError> {
    let millis = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_err(|_| StorageError::adapter("system clock precedes the Unix epoch"))?
        .as_millis();
    let millis = u64::try_from(millis)
        .map_err(|_| StorageError::invalid_input("system clock exceeds the durable range"))?;
    crate::instant_from_millis(millis)
}

fn storage_authority_error(error: &StorageError) -> DeliveryAuthorityError {
    DeliveryAuthorityError::new(error.to_string())
}

fn dispatch_error(error: &StorageError) -> DeliveryExecutionPortError {
    DeliveryExecutionPortError::new(error.to_string())
}

#[cfg(test)]
mod tests {
    use super::{MAX_SAFE_INTEGER, bounded_authority_time};

    #[test]
    fn authority_clock_rejects_a_delivery_at_the_durable_limit() {
        assert!(bounded_authority_time(1, Some(MAX_SAFE_INTEGER)).is_err());
        assert!(bounded_authority_time(MAX_SAFE_INTEGER + 1, None).is_err());
        assert_eq!(
            bounded_authority_time(100, Some(99)).expect("bounded authority time"),
            100
        );
    }
}
