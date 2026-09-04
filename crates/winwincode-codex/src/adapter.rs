// SPDX-License-Identifier: Apache-2.0

use std::collections::{HashMap, VecDeque};
use std::fmt;
use std::fs::OpenOptions;
use std::io::{Read as _, Write as _};
use std::path::{Component, Path, PathBuf};
use std::process::Stdio;
use std::sync::{Arc, Mutex};
use std::time::Duration;

#[cfg(unix)]
use std::os::unix::fs::{OpenOptionsExt as _, PermissionsExt as _};

use crate::candidate_artifact_outbox::{
    CandidateArtifactAckOutcome, CandidateArtifactAuthority, CandidateArtifactOutbox,
    CandidateArtifactUpload, RetainedCandidateArtifact,
};
use crate::{
    ActionRequestTransport, CodexCoreAdapter, CodexPoll, CodexThreadStart, CodexTurnCompletion,
    DurableExecutionDelivery,
    model_port_client::{ModelChunkDisposition, ModelLeaseAuthority},
};
use codex_apply_patch::{Hunk, parse_patch};
use codex_protocol::approvals::{ApplyPatchApprovalRequestEvent, ExecApprovalRequestEvent};
use codex_protocol::models::MessagePhase;
use codex_protocol::protocol::{Event as CodexEvent, EventMsg as CodexEventMsg};
use codex_protocol::request_user_input::{
    RequestUserInputAnswer, RequestUserInputEvent, RequestUserInputResponse,
};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use sha2::{Digest, Sha256};
use uuid::Uuid;
use winwincode_domain::{
    ApprovalId, CodexThreadId, ExecutionEventId, ExecutionMessageId, ExecutionSequence,
    InputRequestId, Instant, InteractiveInputMode, RequestId, SchemaVersion, SessionIdentity,
    Sha256Digest, WorkerId, WorkerInstanceId, WorkspaceRevision,
};
use winwincode_execution_port::{
    action_enforcement::ActionEnforcementSigningKey,
    action_gateway::ExecutionEnvelopeToken,
    capability_adapter::{CapabilityDescriptor, WorkerCapabilityCatalog},
    change_batch_identity::{derive_change_batch_id, validate_change_batch_identity_derivation},
    generated::{
        ApprovalAction, ApprovalActionCategory, ApprovalDecisionMessage,
        ApprovalDecisionMessageDecision, ApprovalDecisionMessageScope, ApprovalRequestMessage,
        ApprovalRequestMessageKind, ArtifactAckMessage, ArtifactReference, ChangeBatchIdentity,
        ChangeBatchProposal, ChangeBatchProposalEvent, ExecutionEventCategory, ExecutionJob,
        ExecutionOutcomeUsage, ExecutionPortMessage, ExecutionScope, InputRequestMessage,
        InputRequestMessageKind, InputResponseMessage, InputResponseMessageStatus,
        InteractiveInputChoice, JobOutcomeMessage, ModelChunkMessage, ModelGatewayRoute,
        RoleSessionPolicyRoleId, RuntimeEventMessage, RuntimeReplayRequestMessage,
        WorkerCapabilitySet,
    },
    replay::ReplayStore,
    runtime_replay::RuntimeReplayIdentity,
    runtime_trace_outbox::{
        ExecutionMode, ObserverMode, RuntimeTraceDraft, RuntimeTraceFact, RuntimeTraceIdentity,
        RuntimeTraceRetention, SecretSafeTraceSummary, WorkerRuntimeTraceOutbox,
        WorkerRuntimeTraceState,
    },
};
#[cfg(feature = "test-support")]
use winwincode_kernel::KernelEvent;
use winwincode_kernel::{
    ApprovalDecision, ApprovalKind, ApprovalResponse, EventPoll, ExactTurnReconciliation, Kernel,
    KernelOptions, RoleExecutionMode, RoleSessionPolicy, SessionOptions, TurnSubmissionOptions,
};

use crate::action_bridge::{ActionBridgeError, ExecutionPortActionGate};
use crate::helper_release::{HELPER_RELEASE_BINARY_MODE, HelperReleaseManifest, MAX_HELPER_BYTES};
use crate::model_bridge::{
    BridgeError, ExecutionPortModelBridge, ModelRunBinding, SharedAuthoritySource,
};
use crate::outbox::ExecutionOutbox;
use crate::performance::{
    PerformanceOperationCompletion, PerformanceOperationKind, duration_millis,
};
use crate::stage_product::{
    change_batch_proposal_json_schema, migrate_persisted_role_session_policy_v1,
    role_session_policy, stage_product_job_digest, stage_product_prompt,
};
use crate::stage_runtime_projection::{
    StageCommandEnd, StageRuntimeContext, StageRuntimeProjector, StageRuntimeRetention,
    StageTurnCompletion,
};
use crate::store::{
    AdapterStore, AdapterStoreError, StoredApprovalOperation, StoredApprovalOperationKind,
    StoredApprovalOperationState, StoredInputOperation, StoredInputOperationState,
};

const FORMAT_REPAIR_PROMPT: &str = "Return only one corrected JSON object matching the active ChangeBatchProposal schema. Correct the preceding final answer's formatting or patch syntax. Do not call tools, modify files, broaden scope, or perform implementation work.";

const DEFAULT_EVENT_POLL_MILLIS: u64 = 25;
const KERNEL_HOME_DIRECTORY: &str = "kernel-home";
const ROLE_POLICY_V2_MIGRATION: &str = "role-session-policy-v1-to-v2";

/// Unvalidated caller-owned production options.
pub struct ProductionCodexOptions {
    pub data_directory: PathBuf,
    pub helper_executable: PathBuf,
    pub helper_release_manifest: HelperReleaseManifest,
    pub provider: String,
    pub model: String,
    pub gateway_route: ModelGatewayRoute,
    pub registered_capabilities: WorkerCapabilitySet,
    pub discovered_capabilities: Vec<CapabilityDescriptor>,
    pub action_signing_key: ActionEnforcementSigningKey,
    pub execution_envelope: ExecutionEnvelopeToken,
    pub execution_mode: ExecutionMode,
    pub observer_mode: ObserverMode,
}

/// Validated immutable adapter configuration.
#[derive(Clone)]
pub struct ProductionCodexConfig {
    data_directory: PathBuf,
    kernel_home: PathBuf,
    helper_executable: PathBuf,
    helper_bytes: Arc<[u8]>,
    helper_release_manifest: HelperReleaseManifest,
    provider: String,
    model: String,
    gateway_route: ModelGatewayRoute,
    registered_capabilities: WorkerCapabilitySet,
    discovered_capabilities: Vec<CapabilityDescriptor>,
    action_signing_key: ActionEnforcementSigningKey,
    execution_envelope: ExecutionEnvelopeToken,
    execution_mode: ExecutionMode,
    observer_mode: ObserverMode,
    event_poll_timeout: Duration,
    #[cfg(feature = "test-support")]
    event_poll_faults: VecDeque<ProductionEventPollFault>,
    #[cfg(feature = "test-support")]
    submission_faults: VecDeque<ProductionSubmissionFault>,
    #[cfg(feature = "test-support")]
    format_repair_faults: VecDeque<ProductionFormatRepairFault>,
}

/// Test-only faults injected at the embedded Kernel event boundary.
#[cfg(feature = "test-support")]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ProductionEventPollFault {
    Closed,
    MalformedEvent,
    KernelError,
    /// A valid Codex `ErrorEvent` before any `TurnStarted` event.
    ErrorEvent,
}

/// Test-only crash boundary around the durable submission intent.
#[cfg(feature = "test-support")]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ProductionSubmissionFault {
    AfterIntentBeforeKernel,
}

/// Test-only failures at the exact format-repair reconciliation boundary.
#[cfg(feature = "test-support")]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ProductionFormatRepairFault {
    KernelRecoveryFailed,
}

impl ProductionCodexConfig {
    /// Validates all process-owned paths, model routing, and Worker capability
    /// inputs before a database or Codex service is opened.
    ///
    /// # Errors
    ///
    /// Rejects relative or missing paths, blank routing fields, and malformed
    /// capability discovery.
    pub fn try_new(options: ProductionCodexOptions) -> Result<Self, ProductionCodexError> {
        if !options.data_directory.is_absolute()
            || !options.helper_executable.is_absolute()
            || !valid_route_token(&options.provider)
            || !valid_route_token(&options.model)
            || !valid_route_token(&options.gateway_route.route)
            || !valid_route_token(&options.gateway_route.capability)
            || options.execution_envelope.version == 0
            || !valid_sha256_digest(&options.execution_envelope.digest.0)
        {
            return Err(ProductionCodexError::new(
                ProductionCodexErrorKind::InvalidConfiguration,
                "production Codex configuration is invalid",
            ));
        }
        let helper_bytes =
            project_helper(&options.helper_executable, &options.helper_release_manifest)
                .ok_or_else(invalid_configuration)?;
        WorkerCapabilityCatalog::discover(
            &options.registered_capabilities,
            options.discovered_capabilities.clone(),
        )
        .map_err(|_| {
            ProductionCodexError::new(
                ProductionCodexErrorKind::InvalidConfiguration,
                "production Codex capability discovery is invalid",
            )
        })?;
        let kernel_home = options.data_directory.join(KERNEL_HOME_DIRECTORY);
        Ok(Self {
            data_directory: options.data_directory,
            kernel_home,
            helper_executable: options.helper_executable,
            helper_bytes,
            helper_release_manifest: options.helper_release_manifest,
            provider: options.provider,
            model: options.model,
            gateway_route: options.gateway_route,
            registered_capabilities: options.registered_capabilities,
            discovered_capabilities: options.discovered_capabilities,
            action_signing_key: options.action_signing_key,
            execution_envelope: options.execution_envelope,
            execution_mode: options.execution_mode,
            observer_mode: options.observer_mode,
            event_poll_timeout: Duration::from_millis(DEFAULT_EVENT_POLL_MILLIS),
            #[cfg(feature = "test-support")]
            event_poll_faults: VecDeque::new(),
            #[cfg(feature = "test-support")]
            submission_faults: VecDeque::new(),
            #[cfg(feature = "test-support")]
            format_repair_faults: VecDeque::new(),
        })
    }

    /// Injects one exact event-boundary fault before the next Kernel poll.
    #[cfg(feature = "test-support")]
    #[must_use]
    pub fn with_test_event_poll_fault(mut self, fault: ProductionEventPollFault) -> Self {
        self.event_poll_faults.push_back(fault);
        self
    }

    /// Stops one submission after its sealed intent is durable and before any Kernel call.
    #[cfg(feature = "test-support")]
    #[must_use]
    pub fn with_test_submission_fault(mut self, fault: ProductionSubmissionFault) -> Self {
        self.submission_faults.push_back(fault);
        self
    }

    /// Fails one exact delegated format-repair reconciliation without opening
    /// another Provider exchange.
    #[cfg(feature = "test-support")]
    #[must_use]
    pub fn with_test_format_repair_fault(mut self, fault: ProductionFormatRepairFault) -> Self {
        self.format_repair_faults.push_back(fault);
        self
    }

    #[must_use]
    pub fn data_directory(&self) -> &Path {
        &self.data_directory
    }

    /// Absolute project-owned helper passed to both Codex self-exec and
    /// `ExecServerRuntimePaths` by the embedded Kernel.
    #[must_use]
    pub fn helper_executable(&self) -> &Path {
        &self.helper_executable
    }

    /// Canonical Provider Gateway route used for all model exchanges.
    #[must_use]
    pub const fn gateway_route(&self) -> &ModelGatewayRoute {
        &self.gateway_route
    }

    #[must_use]
    pub const fn execution_mode(&self) -> ExecutionMode {
        self.execution_mode
    }

    #[must_use]
    pub const fn observer_mode(&self) -> ObserverMode {
        self.observer_mode
    }
}

impl fmt::Debug for ProductionCodexConfig {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("ProductionCodexConfig")
            .field("provider", &self.provider)
            .field("model", &self.model)
            .field("gateway_route", &self.gateway_route)
            .field("execution_mode", &self.execution_mode)
            .field("observer_mode", &self.observer_mode)
            .finish_non_exhaustive()
    }
}

/// Concrete Worker governance surfaces installed beside the embedded Kernel.
pub struct ProductionCodexInstallation {
    capability_catalog: WorkerCapabilityCatalog,
    runtime_trace_outbox: WorkerRuntimeTraceOutbox,
}

impl ProductionCodexInstallation {
    #[must_use]
    pub const fn capability_catalog(&self) -> &WorkerCapabilityCatalog {
        &self.capability_catalog
    }

    #[must_use]
    pub const fn runtime_trace_outbox(&self) -> WorkerRuntimeTraceOutbox {
        self.runtime_trace_outbox
    }
}

impl fmt::Debug for ProductionCodexInstallation {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("ProductionCodexInstallation")
            .field(
                "capability_adapter_version",
                &self.capability_catalog.adapter_version(),
            )
            .field(
                "capability_catalog_digest",
                &self.capability_catalog.catalog_digest(),
            )
            .field("runtime_trace_outbox", &"installed")
            .field("action_enforcement", &"installed")
            .finish_non_exhaustive()
    }
}

/// Sole production [`CodexCoreAdapter`] for `WorkerMain`.
pub struct ProductionCodexAdapter {
    config: ProductionCodexConfig,
    kernel: Arc<Kernel>,
    bridge: Arc<ExecutionPortModelBridge>,
    action_gate: Arc<ExecutionPortActionGate>,
    store: AdapterStore,
    outbox: ExecutionOutbox,
    candidate_artifacts: CandidateArtifactOutbox,
    stage_projector: StageRuntimeProjector,
    installation: ProductionCodexInstallation,
    runs: HashMap<String, ActiveRun>,
    thread_to_run: HashMap<String, String>,
    /// Sequence of the last duplicate model frame accepted by the bridge.
    /// A duplicate first frame can arrive after the original `ModelOpen` was
    /// compacted, so its open acknowledgement is an idempotent no-op rather
    /// than a Worker-fatal missing-row conflict.  Keeping the sequence here
    /// also prevents a later unrelated frame from consuming that fact.
    last_duplicate_model_chunk_sequence: Option<i64>,
}

impl ProductionCodexAdapter {
    /// Opens durable Worker-owned state and links one embedded Kernel to the
    /// `WorkerModelPortClient` bridge.
    ///
    /// # Errors
    ///
    /// Fails closed when durable state, capability installation, receipt use
    /// state, or the embedded Kernel cannot be opened.
    pub fn open(mut config: ProductionCodexConfig) -> Result<Self, ProductionCodexError> {
        // Establish the two process-owned roots before opening any database,
        // action gate, or Core state.  In particular, do not let
        // `create_dir_all` follow a caller-provided final symlink and then
        // chmod an unrelated directory.  The same check is repeated by the
        // Kernel for its own root and by the store for its database files.
        ensure_private_directory(&config.data_directory).map_err(|_| unavailable())?;
        ensure_private_directory(&config.kernel_home).map_err(|_| unavailable())?;
        config.helper_executable = seal_helper(
            &config.helper_executable,
            Some(config.helper_bytes.as_ref()),
            &config.data_directory,
            &config.helper_release_manifest,
        )?;
        let capability_catalog = WorkerCapabilityCatalog::discover(
            &config.registered_capabilities,
            config.discovered_capabilities.clone(),
        )
        .map_err(|_| invalid_configuration())?;
        let store = AdapterStore::open(&config.data_directory).map_err(map_store_error)?;
        migrate_stored_run_role_policies_v1_to_v2(&store)?;
        let outbox = ExecutionOutbox::open(store.clone()).map_err(map_store_error)?;
        let candidate_artifacts =
            CandidateArtifactOutbox::open(store.clone()).map_err(map_store_error)?;
        let authority = SharedAuthoritySource::default();
        let bridge = Arc::new(ExecutionPortModelBridge::new(
            store.clone(),
            outbox.clone(),
            config.gateway_route.clone(),
            config.provider.clone(),
            authority,
        ));
        let action_gate = Arc::new(
            ExecutionPortActionGate::open(
                &config.data_directory,
                capability_catalog.clone(),
                config.execution_envelope.clone(),
                config.action_signing_key.clone(),
            )
            .map_err(|_| unavailable())?,
        );
        bridge
            .attach_action_gate(action_gate.clone())
            .map_err(map_bridge_error)?;
        let kernel = Arc::new(
            Kernel::new(
                KernelOptions::new(config.kernel_home.clone(), config.helper_executable.clone()),
                bridge.model_port(),
                action_gate.clone(),
            )
            .map_err(|_| {
                ProductionCodexError::new(
                    ProductionCodexErrorKind::Kernel,
                    "embedded Codex Kernel could not start",
                )
            })?,
        );
        Ok(Self {
            config,
            kernel,
            bridge,
            action_gate,
            store,
            outbox,
            candidate_artifacts,
            stage_projector: StageRuntimeProjector::new(),
            installation: ProductionCodexInstallation {
                capability_catalog,
                runtime_trace_outbox: WorkerRuntimeTraceOutbox::new(),
            },
            runs: HashMap::new(),
            thread_to_run: HashMap::new(),
            last_duplicate_model_chunk_sequence: None,
        })
    }

    #[must_use]
    pub const fn installation(&self) -> &ProductionCodexInstallation {
        &self.installation
    }

    #[must_use]
    pub fn database_path(&self) -> &Path {
        self.store.path()
    }

    fn run_key_for_thread(&self, thread_id: &CodexThreadId) -> Result<&str, ProductionCodexError> {
        self.thread_to_run
            .get(&thread_id.0)
            .map(String::as_str)
            .ok_or_else(unknown_thread)
    }

    fn session_for_thread(
        &self,
        thread_id: &CodexThreadId,
    ) -> Result<String, ProductionCodexError> {
        let run_key = self.run_key_for_thread(thread_id)?;
        self.runs
            .get(run_key)
            .filter(|run| run.kernel_live)
            .map(|run| run.record.kernel_session_id.clone())
            .ok_or_else(unknown_thread)
    }

    fn retain_runtime_trace(
        &mut self,
        run_key: &str,
        state: WorkerRuntimeTraceState,
        summary: &'static str,
    ) -> Result<Box<RuntimeEventMessage>, ProductionCodexError> {
        self.retain_runtime_trace_at(run_key, state, summary, None)
    }

    /// Retains a lifecycle trace, optionally at an identity already committed
    /// to the terminal record.  The fixed identity is required when recovery
    /// finds `terminal_trace_pending`: a process may have appended the exact
    /// stopped frame and then stopped before recording the final phase.  A
    /// fresh `highest + 1` identity in that window would turn an exact replay
    /// into a false conflict (or emit a second stopped event).
    fn retain_runtime_trace_at(
        &mut self,
        run_key: &str,
        state: WorkerRuntimeTraceState,
        summary: &'static str,
        fixed: Option<&StoredTerminalTrace>,
    ) -> Result<Box<RuntimeEventMessage>, ProductionCodexError> {
        let run = self.runs.get(run_key).ok_or_else(unknown_thread)?;
        let identity = RuntimeReplayIdentity {
            lease: run.binding.authority.lease.clone(),
            worker_session_id: run.binding.authority.worker_session_id.clone(),
            session_identity: run.binding.authority.session_identity.clone(),
            codex_thread_id: run.binding.canonical_thread_id.clone(),
        };
        let stream = identity.stream_key();
        let snapshot = ReplayStore::load(&mut self.store, &stream)
            .map_err(map_store_error)?
            .unwrap_or_default();
        let sequence = fixed.map_or_else(
            || {
                snapshot
                    .highest_sequence
                    .checked_add(1)
                    .ok_or_else(unavailable)
            },
            |trace| u64::try_from(trace.sequence.0).map_err(|_| unavailable()),
        )?;
        let occurred_at = run.record.last_activity_at.clone();
        let event_id = fixed.map_or_else(
            || {
                ExecutionEventId(canonical_id(
                    "xevt",
                    b"codex-runtime-event",
                    run_key,
                    sequence,
                ))
            },
            |trace| trace.event_id.clone(),
        );
        let draft = RuntimeTraceDraft {
            identity: RuntimeTraceIdentity {
                lease: identity.lease,
                worker_session_id: identity.worker_session_id,
                session_identity: identity.session_identity,
                message_id: ExecutionMessageId(canonical_id(
                    "xmsg",
                    b"codex-runtime-message",
                    run_key,
                    sequence,
                )),
                event_id,
                sequence: ExecutionSequence(i64::try_from(sequence).map_err(|_| unavailable())?),
                occurred_at: occurred_at.clone(),
                sent_at: occurred_at,
            },
            category: ExecutionEventCategory::Lifecycle,
            summary: SecretSafeTraceSummary::new(summary).map_err(|_| unavailable())?,
            fact: RuntimeTraceFact::Runtime { state },
            artifacts: Vec::new(),
        };
        match self
            .installation
            .runtime_trace_outbox
            .retain(&mut self.store, &self.bridge.authority(), draft)
            .map_err(|_| unavailable())?
        {
            RuntimeTraceRetention::Ready {
                message, duplicate, ..
            } => {
                // A replay duplicate may already be acknowledged in the
                // runtime stream.  Re-inserting it into the adapter outbox
                // would resend an event that has a durable source frame and
                // violate exact ACK/restart semantics.
                if !duplicate {
                    self.outbox
                        .retain(&ExecutionPortMessage::RuntimeEventMessage(
                            (*message).clone(),
                        ))
                        .map_err(map_store_error)?;
                }
                Ok(message)
            }
            RuntimeTraceRetention::Gap { .. } | RuntimeTraceRetention::Conflict { .. } => {
                Err(unavailable())
            }
        }
    }

    fn retain_stage_projection(
        &mut self,
        run_key: &str,
        source: &str,
        retention: Option<StageRuntimeRetention>,
    ) -> Result<Option<RuntimeEventMessage>, ProductionCodexError> {
        let Some(retention) = retention else {
            return Ok(None);
        };
        let (message, duplicate) = match retention {
            StageRuntimeRetention::Ready { message, duplicate } => (*message, duplicate),
            StageRuntimeRetention::Gap { .. } | StageRuntimeRetention::Conflict { .. } => {
                return Err(conflict());
            }
        };
        // A replay duplicate already exists in the durable runtime stream.
        // Its adapter-outbox row may have been acknowledged and compacted, so
        // retaining it again here would create a second transport attempt for
        // an event whose source marker is already durable.  Pending replay
        // rows are rebuilt from the runtime stream at restart; only the first
        // projection needs a new outbox row.
        if !duplicate {
            self.outbox
                .retain(&ExecutionPortMessage::RuntimeEventMessage(message.clone()))
                .map_err(map_store_error)?;
        }
        let should_persist = {
            let run = self.runs.get_mut(run_key).ok_or_else(unknown_thread)?;
            if run
                .record
                .stage_product_sources
                .iter()
                .any(|retained| retained == source)
            {
                false
            } else {
                run.record.stage_product_sources.push(source.to_owned());
                true
            }
        };
        if should_persist {
            self.persist_run(run_key)?;
        }
        // A crash can occur after the projector appends its frame but before
        // the source marker is committed to `StoredRun`.  The replay store is
        // already the durable source of truth in that case: the pending frame
        // is loaded into `ActiveRun.replay`, or it has already been
        // acknowledged.  Returning the duplicate here would send the same
        // sequence twice and make Worker cursor validation fail, so only the
        // first retention returns a frame to the poller.
        Ok((!duplicate).then_some(message))
    }

    fn stage_context<'context>(
        run: &'context ActiveRun,
        occurred_at: &'context Instant,
        sent_at: &'context Instant,
    ) -> StageRuntimeContext<'context> {
        StageRuntimeContext {
            lease: &run.binding.authority.lease,
            worker_session_id: &run.binding.authority.worker_session_id,
            session_identity: &run.binding.authority.session_identity,
            occurred_at,
            sent_at,
        }
    }

    fn retain_stage_turn_started(
        &mut self,
        run_key: &str,
        turn_id: &str,
        now: &Instant,
    ) -> Result<Option<RuntimeEventMessage>, ProductionCodexError> {
        let source = crate::stage_runtime_projection::source_key("policy", turn_id, None);
        let run = self.runs.get(run_key).ok_or_else(unknown_thread)?;
        let authority = self.bridge.authority();
        let retention = self
            .stage_projector
            .retain_turn_started(
                &mut self.store,
                &authority,
                Self::stage_context(run, now, now),
                &run.record.job,
                turn_id,
            )
            .map_err(|_| unavailable())?;
        self.retain_stage_projection(run_key, &source, retention)
    }

    fn retain_stage_command_end(
        &mut self,
        run_key: &str,
        command_end: &codex_protocol::protocol::ExecCommandEndEvent,
        now: &Instant,
    ) -> Result<Option<RuntimeEventMessage>, ProductionCodexError> {
        let source = crate::stage_runtime_projection::source_key(
            "evidence",
            &command_end.turn_id,
            Some(&command_end.call_id),
        );
        let run = self.runs.get(run_key).ok_or_else(unknown_thread)?;
        let authority = self.bridge.authority();
        let retention = self
            .stage_projector
            .retain_exec_command_end(
                &mut self.store,
                &authority,
                Self::stage_context(run, now, now),
                &run.record.job,
                StageCommandEnd {
                    command: &command_end.command,
                    turn_id: &command_end.turn_id,
                    call_id: &command_end.call_id,
                    status: verification_evidence_status(&command_end.status),
                    exit_code: i64::from(command_end.exit_code),
                },
            )
            .map_err(|_| unavailable())?;
        self.retain_stage_projection(run_key, &source, retention)
    }

    fn retain_stage_turn_completed(
        &mut self,
        run_key: &str,
        turn_id: &str,
        final_message: Option<&str>,
        failed: bool,
        now: &Instant,
    ) -> Result<Option<RuntimeEventMessage>, ProductionCodexError> {
        let source = crate::stage_runtime_projection::source_key("result", turn_id, None);
        let run = self.runs.get(run_key).ok_or_else(unknown_thread)?;
        let authority = self.bridge.authority();
        let retention = self
            .stage_projector
            .retain_turn_completed(
                &mut self.store,
                &authority,
                Self::stage_context(run, now, now),
                &run.record.job,
                StageTurnCompletion {
                    turn_id,
                    final_message,
                    failed,
                },
            )
            .map_err(|_| unavailable())?;
        self.retain_stage_projection(run_key, &source, retention)
    }

    /// Converts a rejected semantic stage product into the normal durable
    /// terminal path.  The Worker only sees this path after the stopped trace
    /// is retained, so a malformed model result can never be mistaken for a
    /// successful `Outcome`, and a poll retry does not keep invoking Codex.
    fn retain_stage_failure(
        &mut self,
        run_key: &str,
        now: &Instant,
    ) -> Result<CodexPoll, ProductionCodexError> {
        self.store
            .commit_provider_final_model_calls(run_key)
            .map_err(map_store_error)?;
        let first_failure = self
            .runs
            .get(run_key)
            .ok_or_else(unknown_thread)?
            .record
            .terminal
            .is_none();
        if first_failure {
            let run = self.runs.get_mut(run_key).ok_or_else(unknown_thread)?;
            run.record.last_activity_at = now.clone();
            run.record.terminal = Some(StoredTerminal::Failed);
            run.record.phase = StoredRunPhase::TerminalTracePending;
            self.persist_run(run_key)?;
        }
        self.poll_retained_terminal(run_key)?
            .ok_or_else(unavailable)
    }

    fn persist_run(&self, run_key: &str) -> Result<(), ProductionCodexError> {
        let run = self.runs.get(run_key).ok_or_else(unknown_thread)?;
        self.store
            .save_run(run_key, &run.record)
            .map_err(map_store_error)
    }

    fn poll_retained_terminal(
        &mut self,
        run_key: &str,
    ) -> Result<Option<CodexPoll>, ProductionCodexError> {
        let Some((terminal, phase)) = self.runs.get(run_key).and_then(|run| {
            run.record
                .terminal
                .clone()
                .map(|terminal| (terminal, run.record.phase))
        }) else {
            return Ok(None);
        };
        if phase == StoredRunPhase::TerminalTracePending {
            let baseline_retained = self
                .store
                .load_performance_projection(run_key)
                .map_err(map_store_error)?
                .is_some_and(|projection| projection.retained);
            let trace = if baseline_retained {
                self.retain_terminal_trace(run_key, terminal.trace_summary())?
            } else {
                self.retain_performance_baseline_trace(run_key)?
            };
            return Ok(Some(CodexPoll::RuntimeTrace(trace)));
        }
        terminal.into_poll().map(Some)
    }

    fn retain_terminal_trace(
        &mut self,
        run_key: &str,
        summary: &'static str,
    ) -> Result<Box<RuntimeEventMessage>, ProductionCodexError> {
        if self
            .runs
            .get(run_key)
            .ok_or_else(unknown_thread)?
            .record
            .terminal_trace
            .is_none()
        {
            let run = self.runs.get(run_key).ok_or_else(unknown_thread)?;
            let identity = RuntimeReplayIdentity {
                lease: run.binding.authority.lease.clone(),
                worker_session_id: run.binding.authority.worker_session_id.clone(),
                session_identity: run.binding.authority.session_identity.clone(),
                codex_thread_id: run.binding.canonical_thread_id.clone(),
            };
            let snapshot = ReplayStore::load(&mut self.store, &identity.stream_key())
                .map_err(map_store_error)?
                .unwrap_or_default();
            let sequence = snapshot
                .highest_sequence
                .checked_add(1)
                .ok_or_else(unavailable)?;
            let trace = StoredTerminalTrace {
                event_id: ExecutionEventId(canonical_id(
                    "xevt",
                    b"codex-runtime-event",
                    run_key,
                    sequence,
                )),
                sequence: ExecutionSequence(i64::try_from(sequence).map_err(|_| unavailable())?),
                retained: false,
            };
            self.runs
                .get_mut(run_key)
                .ok_or_else(unknown_thread)?
                .record
                .terminal_trace = Some(trace);
            self.persist_run(run_key)?;
        }
        let terminal_trace = self
            .runs
            .get(run_key)
            .ok_or_else(unknown_thread)?
            .record
            .terminal_trace
            .clone()
            .ok_or_else(unavailable)?;
        let message = self.retain_runtime_trace_at(
            run_key,
            WorkerRuntimeTraceState::Stopped,
            summary,
            Some(&terminal_trace),
        )?;
        let run = self.runs.get_mut(run_key).ok_or_else(unknown_thread)?;
        let retained = run.record.terminal_trace.as_mut().ok_or_else(unavailable)?;
        if retained.event_id != message.event.event_id
            || retained.sequence != message.event.sequence
        {
            return Err(conflict());
        }
        retained.retained = true;
        run.record.phase = StoredRunPhase::Terminal;
        self.persist_run(run_key)?;
        Ok(message)
    }

    fn retain_performance_baseline_trace(
        &mut self,
        run_key: &str,
    ) -> Result<Box<RuntimeEventMessage>, ProductionCodexError> {
        let projection = if let Some(projection) = self
            .store
            .load_performance_projection(run_key)
            .map_err(map_store_error)?
        {
            projection
        } else {
            let run = self.runs.get(run_key).ok_or_else(unknown_thread)?;
            let identity = RuntimeReplayIdentity {
                lease: run.binding.authority.lease.clone(),
                worker_session_id: run.binding.authority.worker_session_id.clone(),
                session_identity: run.binding.authority.session_identity.clone(),
                codex_thread_id: run.binding.canonical_thread_id.clone(),
            };
            let snapshot = ReplayStore::load(&mut self.store, &identity.stream_key())
                .map_err(map_store_error)?
                .unwrap_or_default();
            let sequence = snapshot
                .highest_sequence
                .checked_add(1)
                .ok_or_else(unavailable)?;
            let report = self
                .store
                .performance_report(run_key, run.record.last_runtime_millis)
                .map_err(map_store_error)?;
            self.store
                .reserve_performance_projection(
                    run_key,
                    ExecutionEventId(canonical_id(
                        "xevt",
                        b"codex-performance-baseline",
                        run_key,
                        sequence,
                    )),
                    ExecutionSequence(i64::try_from(sequence).map_err(|_| unavailable())?),
                    report,
                )
                .map_err(map_store_error)?
        };
        let run = self.runs.get(run_key).ok_or_else(unknown_thread)?;
        let occurred_at = run.record.last_activity_at.clone();
        let sequence = u64::try_from(projection.sequence.0).map_err(|_| unavailable())?;
        let draft = RuntimeTraceDraft {
            identity: RuntimeTraceIdentity {
                lease: run.binding.authority.lease.clone(),
                worker_session_id: run.binding.authority.worker_session_id.clone(),
                session_identity: run.binding.authority.session_identity.clone(),
                message_id: ExecutionMessageId(canonical_id(
                    "xmsg",
                    b"codex-performance-message",
                    run_key,
                    sequence,
                )),
                event_id: projection.event_id,
                sequence: projection.sequence,
                occurred_at: occurred_at.clone(),
                sent_at: occurred_at,
            },
            category: ExecutionEventCategory::Usage,
            summary: SecretSafeTraceSummary::new("execution performance baseline recorded")
                .map_err(|_| unavailable())?,
            fact: RuntimeTraceFact::PerformanceBaseline {
                report: projection.report,
            },
            artifacts: Vec::new(),
        };
        let message = match self
            .installation
            .runtime_trace_outbox
            .retain(&mut self.store, &self.bridge.authority(), draft)
            .map_err(|_| unavailable())?
        {
            RuntimeTraceRetention::Ready {
                message, duplicate, ..
            } => {
                if !duplicate {
                    self.outbox
                        .retain(&ExecutionPortMessage::RuntimeEventMessage(
                            (*message).clone(),
                        ))
                        .map_err(map_store_error)?;
                }
                message
            }
            RuntimeTraceRetention::Gap { .. } | RuntimeTraceRetention::Conflict { .. } => {
                return Err(unavailable());
            }
        };
        self.store
            .mark_performance_projection_retained(run_key)
            .map_err(map_store_error)?;
        Ok(message)
    }

    async fn poll_infrastructure_terminal(
        &mut self,
        run_key: &str,
        now: &Instant,
    ) -> Result<CodexPoll, ProductionCodexError> {
        let first_failure = self
            .runs
            .get(run_key)
            .ok_or_else(unknown_thread)?
            .record
            .terminal
            .is_none();
        if first_failure {
            {
                let run = self.runs.get_mut(run_key).ok_or_else(unknown_thread)?;
                run.record.last_activity_at = now.clone();
                run.record.terminal = Some(StoredTerminal::InfrastructureFailed);
                run.record.phase = StoredRunPhase::TerminalTracePending;
            }
            self.persist_run(run_key)?;
            self.quiesce_infrastructure_run(run_key, now).await;
        }
        self.poll_retained_terminal(run_key)?
            .ok_or_else(unavailable)
    }

    /// Converts a failed exact submission into the same durable terminal path
    /// used by event-poll failures. The Worker can then flush the retained
    /// `Stopped` trace before retaining the final infrastructure outcome.
    async fn retain_submission_failure(
        &mut self,
        run_key: &str,
    ) -> Result<(), ProductionCodexError> {
        let activity_at = self
            .runs
            .get(run_key)
            .ok_or_else(unknown_thread)?
            .record
            .last_activity_at
            .clone();
        let first_failure = self
            .runs
            .get(run_key)
            .ok_or_else(unknown_thread)?
            .record
            .terminal
            .is_none();
        if !first_failure {
            return Ok(());
        }
        {
            let run = self.runs.get_mut(run_key).ok_or_else(unknown_thread)?;
            run.record.terminal = Some(StoredTerminal::InfrastructureFailed);
            run.record.phase = StoredRunPhase::TerminalTracePending;
        }
        self.persist_run(run_key)?;
        self.quiesce_infrastructure_run(run_key, &activity_at).await;
        let _ = self.retain_performance_baseline_trace(run_key)?;
        let _ = self.retain_terminal_trace(run_key, "embedded Codex infrastructure failure")?;
        Ok(())
    }

    async fn quiesce_infrastructure_run(&mut self, run_key: &str, now: &Instant) {
        let Some(run) = self.runs.get(run_key) else {
            return;
        };
        let thread_id = run.binding.canonical_thread_id.clone();
        let session_id = run.record.kernel_session_id.clone();
        let kernel_live = run.kernel_live;
        let _ = self.bridge.cancel_thread(&thread_id, now).await;
        let _ = self.bridge.discard_messages_for_thread(&thread_id);
        let _ = self.action_gate.cancel_session(&session_id);
        if kernel_live {
            let _ = self.kernel.close_session(&session_id).await;
            if let Some(run) = self.runs.get_mut(run_key) {
                run.kernel_live = false;
            }
        }
        // A model-port task can finish its in-flight open while Core is
        // shutting down.  Discard once more after the shutdown barrier so a
        // late ModelOpen/ModelAck cannot escape a terminal infrastructure
        // path.
        let _ = self.bridge.discard_messages_for_thread(&thread_id);
    }

    async fn next_kernel_event(&mut self, session: &str) -> Result<EventPoll, ()> {
        #[cfg(feature = "test-support")]
        if let Some(fault) = self.config.event_poll_faults.pop_front() {
            return match fault {
                ProductionEventPollFault::Closed => Ok(EventPoll::Closed),
                ProductionEventPollFault::MalformedEvent => Ok(EventPoll::Event(KernelEvent {
                    sequence: 1,
                    kind: "malformed_test_event".to_owned(),
                    payload_json: "{".to_owned(),
                })),
                ProductionEventPollFault::KernelError => Err(()),
                ProductionEventPollFault::ErrorEvent => Ok(EventPoll::Event(KernelEvent {
                    sequence: 1,
                    kind: "error".to_owned(),
                    payload_json: serde_json::json!({
                        "id": "winwincode-test-error",
                        "msg": {
                            "type": "error",
                            "message": "deterministic pre-start failure",
                            "codex_error_info": null,
                        },
                    })
                    .to_string(),
                })),
            };
        }
        self.kernel
            .next_event(session, Some(self.config.event_poll_timeout))
            .await
            .map_err(|_| ())
    }

    fn accept_polled_event(
        &mut self,
        run_key: &str,
        event: CodexEvent,
        now: &Instant,
    ) -> Result<CodexPoll, ProductionCodexError> {
        if self.record_standalone_performance_event(run_key, &event.msg, now)? {
            return Ok(CodexPoll::Pending);
        }
        match event.msg {
            CodexEventMsg::TurnStarted(started) => {
                self.record_performance_start(
                    run_key,
                    PerformanceOperationKind::Turn,
                    &started.turn_id,
                    now,
                )?;
                self.accept_turn_started(run_key, &started.turn_id, now)
            }
            CodexEventMsg::TokenCount(token_count) => {
                if let Some(info) = token_count.info {
                    let run = self.runs.get_mut(run_key).ok_or_else(unknown_thread)?;
                    run.record.last_tokens = info.last_token_usage.total_tokens.max(0);
                    self.persist_run(run_key)?;
                }
                Ok(CodexPoll::Pending)
            }
            CodexEventMsg::TurnComplete(completed) => {
                self.accept_turn_complete(run_key, &completed, now)
            }
            CodexEventMsg::AgentMessage(message) => {
                if message.phase != Some(MessagePhase::FinalAnswer) {
                    return Ok(CodexPoll::Pending);
                }
                {
                    let run = self.runs.get_mut(run_key).ok_or_else(unknown_thread)?;
                    run.record.last_agent_message = Some(message.message.clone());
                    run.record.last_activity_at = now.clone();
                }
                self.persist_run(run_key)?;
                // Core emits the terminal TurnComplete event after the final
                // AgentMessage.  Keep this observation durable, then let the
                // terminal event perform the role-specific stage projection so
                // a transient final-message frame cannot close a turn before
                // Core has committed its exact terminal record.
                Ok(CodexPoll::Pending)
            }
            CodexEventMsg::ExecCommandEnd(command_end) => {
                self.accept_command_end(run_key, &command_end, now)
            }
            CodexEventMsg::ExecApprovalRequest(request) => {
                let message = self.retain_exec_approval_request(run_key, &request)?;
                self.outbox
                    .retain(&ExecutionPortMessage::ApprovalRequestMessage(
                        message.clone(),
                    ))
                    .map_err(map_store_error)?;
                self.action_gate
                    .enqueue_message(ExecutionPortMessage::ApprovalRequestMessage(message))
                    .map_err(|_| unavailable())?;
                Ok(CodexPoll::Pending)
            }
            CodexEventMsg::ApplyPatchApprovalRequest(request) => {
                let message = self.retain_patch_approval_request(run_key, &request)?;
                self.outbox
                    .retain(&ExecutionPortMessage::ApprovalRequestMessage(
                        message.clone(),
                    ))
                    .map_err(map_store_error)?;
                self.action_gate
                    .enqueue_message(ExecutionPortMessage::ApprovalRequestMessage(message))
                    .map_err(|_| unavailable())?;
                Ok(CodexPoll::Pending)
            }
            CodexEventMsg::RequestUserInput(request) => {
                if let Some(message) = self.retain_input_request(run_key, &request)? {
                    self.outbox
                        .retain(&ExecutionPortMessage::InputRequestMessage(message.clone()))
                        .map_err(map_store_error)?;
                    self.action_gate
                        .enqueue_message(ExecutionPortMessage::InputRequestMessage(message))
                        .map_err(|_| unavailable())?;
                }
                Ok(CodexPoll::Pending)
            }
            CodexEventMsg::Error(_error) => self.accept_error(run_key, now),
            _ => Ok(CodexPoll::Pending),
        }
    }

    fn record_standalone_performance_event(
        &self,
        run_key: &str,
        event: &CodexEventMsg,
        now: &Instant,
    ) -> Result<bool, ProductionCodexError> {
        match event {
            CodexEventMsg::ExecCommandBegin(call) => {
                if matches!(
                    call.source,
                    codex_protocol::protocol::ExecCommandSource::UserShell
                ) {
                    Ok(true)
                } else {
                    self.record_tool_start(run_key, &call.call_id, now)
                }
            }
            CodexEventMsg::PatchApplyBegin(call) => {
                self.record_patch_start(run_key, &call.call_id, now)
            }
            CodexEventMsg::PatchApplyEnd(call) => self.record_patch_completion(run_key, call, now),
            CodexEventMsg::McpToolCallBegin(call) => {
                self.record_tool_start(run_key, &call.call_id, now)
            }
            CodexEventMsg::McpToolCallEnd(call) => self.record_tool_completion(
                run_key,
                &call.call_id,
                now,
                Some(duration_millis(call.duration)),
            ),
            CodexEventMsg::WebSearchBegin(call) => {
                self.record_tool_start(run_key, &call.call_id, now)
            }
            CodexEventMsg::WebSearchEnd(call) => {
                self.record_tool_completion(run_key, &call.call_id, now, None)
            }
            CodexEventMsg::ImageGenerationBegin(call) => {
                self.record_tool_start(run_key, &call.call_id, now)
            }
            CodexEventMsg::ImageGenerationEnd(call) => {
                self.record_tool_completion(run_key, &call.call_id, now, None)
            }
            CodexEventMsg::ViewImageToolCall(call) => {
                self.record_tool_start(run_key, &call.call_id, now)?;
                self.record_tool_completion(run_key, &call.call_id, now, Some(0))
            }
            _ => Ok(false),
        }
    }

    fn accept_turn_started(
        &mut self,
        run_key: &str,
        turn_id: &str,
        now: &Instant,
    ) -> Result<CodexPoll, ProductionCodexError> {
        let repair_turn = self.runs.get(run_key).is_some_and(|run| {
            run.record
                .format_repair
                .as_ref()
                .is_some_and(|repair| repair.turn_id == turn_id)
        });
        if repair_turn {
            let run = self.runs.get_mut(run_key).ok_or_else(unknown_thread)?;
            run.record.current_turn_id = Some(turn_id.to_owned());
            run.record.last_activity_at = now.clone();
            if let Some(repair) = run.record.format_repair.as_mut() {
                repair.submitted = true;
            }
            self.persist_run(run_key)?;
            return Ok(CodexPoll::Pending);
        }
        if self
            .runs
            .get(run_key)
            .is_some_and(|run| run.record.phase == StoredRunPhase::RuntimeStarted)
        {
            return Ok(CodexPoll::Pending);
        }
        {
            let run = self.runs.get_mut(run_key).ok_or_else(unknown_thread)?;
            run.record.current_turn_id = Some(turn_id.to_owned());
            run.record.last_activity_at = now.clone();
        }
        self.persist_run(run_key)?;
        let Ok(stage) = self.retain_stage_turn_started(run_key, turn_id, now) else {
            return self.retain_stage_failure(run_key, now);
        };
        let trace = self.retain_runtime_trace(
            run_key,
            WorkerRuntimeTraceState::Started,
            "embedded Codex turn started",
        )?;
        self.runs
            .get_mut(run_key)
            .ok_or_else(unknown_thread)?
            .record
            .phase = StoredRunPhase::RuntimeStarted;
        self.persist_run(run_key)?;
        Ok(stage.map_or(CodexPoll::RuntimeTrace(trace), |stage| {
            // WorkerMain flushes the generic trace after forwarding this
            // semantic stage event; do not retain another in-memory copy.
            CodexPoll::RuntimeTrace(Box::new(stage))
        }))
    }

    fn accept_turn_complete(
        &mut self,
        run_key: &str,
        completed: &codex_protocol::protocol::TurnCompleteEvent,
        now: &Instant,
    ) -> Result<CodexPoll, ProductionCodexError> {
        let (failed, final_message) = {
            let run = self.runs.get_mut(run_key).ok_or_else(unknown_thread)?;
            run.record.current_turn_id = Some(completed.turn_id.clone());
            run.record.last_activity_at = now.clone();
            run.record.last_runtime_millis = completed.duration_ms.unwrap_or(0).max(0);
            if completed.last_agent_message.is_some() {
                run.record
                    .last_agent_message
                    .clone_from(&completed.last_agent_message);
            }
            (
                completed.error.is_some(),
                completed
                    .last_agent_message
                    .clone()
                    .or_else(|| run.record.last_agent_message.clone()),
            )
        };
        self.persist_run(run_key)?;
        if self
            .runs
            .get(run_key)
            .is_some_and(|run| is_delegated_composer(&run.record))
        {
            self.store
                .commit_provider_final_model_calls(run_key)
                .map_err(map_store_error)?;
            if failed {
                if self
                    .runs
                    .get(run_key)
                    .is_some_and(|run| is_format_repair_turn(&run.record, &completed.turn_id))
                {
                    return self.retain_delegated_repair_infrastructure_failure(run_key, now);
                }
                return self.retain_delegated_inconclusive(run_key, now);
            }
            return self.accept_delegated_final_output(
                run_key,
                &completed.turn_id,
                final_message.as_deref(),
                now,
            );
        }
        let Ok(stage) = self.retain_stage_turn_completed(
            run_key,
            &completed.turn_id,
            final_message.as_deref(),
            failed,
            now,
        ) else {
            return self.retain_stage_failure(run_key, now);
        };
        self.store
            .commit_provider_final_model_calls(run_key)
            .map_err(map_store_error)?;
        let run = self.runs.get_mut(run_key).ok_or_else(unknown_thread)?;
        run.record.terminal = Some(if failed {
            StoredTerminal::Failed
        } else {
            StoredTerminal::Completed {
                summary: "embedded Codex turn completed".to_owned(),
                final_message,
                artifacts: Vec::new(),
                usage: ExecutionOutcomeUsage {
                    runtime_millis: run.record.last_runtime_millis,
                    tokens: run.record.last_tokens,
                    cost_microunits: 0,
                },
            }
        });
        run.record.phase = StoredRunPhase::TerminalTracePending;
        self.persist_run(run_key)?;
        if let Some(stage) = stage {
            Ok(CodexPoll::RuntimeTrace(Box::new(stage)))
        } else {
            self.poll_retained_terminal(run_key)?
                .ok_or_else(unavailable)
        }
    }

    fn accept_delegated_final_output(
        &mut self,
        run_key: &str,
        turn_id: &str,
        final_message: Option<&str>,
        now: &Instant,
    ) -> Result<CodexPoll, ProductionCodexError> {
        let event = {
            let run = self.runs.get(run_key).ok_or_else(unknown_thread)?;
            delegated_change_batch_event(&run.record, &run.binding, turn_id, final_message, now)
        };
        if let Ok(event) = event {
            let run = self.runs.get_mut(run_key).ok_or_else(unknown_thread)?;
            let event = if let Some(existing) = &run.record.batch_intent {
                if existing.event.identity != event.identity
                    || existing.event.proposal != event.proposal
                {
                    return Err(conflict());
                }
                existing.event.clone()
            } else {
                run.record.batch_intent = Some(StoredBatchIntent {
                    event: event.clone(),
                });
                event
            };
            self.persist_run(run_key)?;
            self.runs
                .get_mut(run_key)
                .ok_or_else(unknown_thread)?
                .batch_intent_emission = OneShotState::Consumed;
            Ok(CodexPoll::ChangeBatchProposed(Box::new(event)))
        } else {
            let already_repairing = self
                .runs
                .get(run_key)
                .ok_or_else(unknown_thread)?
                .record
                .format_repair
                .is_some();
            if already_repairing {
                self.retain_delegated_inconclusive(run_key, now)
            } else {
                let repair_turn_id = canonical_parts_id(
                    "trn",
                    b"winwincode.delegated-format-repair.v1",
                    &[run_key.as_bytes(), turn_id.as_bytes()],
                );
                let run = self.runs.get_mut(run_key).ok_or_else(unknown_thread)?;
                run.record.format_repair = Some(StoredFormatRepair {
                    turn_id: repair_turn_id,
                    submitted: false,
                });
                self.persist_run(run_key)?;
                Ok(CodexPoll::Pending)
            }
        }
    }

    fn retain_delegated_inconclusive(
        &mut self,
        run_key: &str,
        now: &Instant,
    ) -> Result<CodexPoll, ProductionCodexError> {
        let run = self.runs.get_mut(run_key).ok_or_else(unknown_thread)?;
        run.record.last_activity_at = now.clone();
        run.record.terminal = Some(StoredTerminal::DelegatedInconclusive);
        run.record.phase = StoredRunPhase::TerminalTracePending;
        self.persist_run(run_key)?;
        self.poll_retained_terminal(run_key)?
            .ok_or_else(unavailable)
    }

    fn retain_delegated_repair_infrastructure_failure(
        &mut self,
        run_key: &str,
        now: &Instant,
    ) -> Result<CodexPoll, ProductionCodexError> {
        self.store
            .commit_provider_final_model_calls(run_key)
            .map_err(map_store_error)?;
        let run = self.runs.get_mut(run_key).ok_or_else(unknown_thread)?;
        run.record.last_activity_at = now.clone();
        run.record.terminal = Some(StoredTerminal::DelegatedRepairInfrastructureFailed);
        run.record.phase = StoredRunPhase::TerminalTracePending;
        self.persist_run(run_key)?;
        self.poll_retained_terminal(run_key)?
            .ok_or_else(unavailable)
    }

    async fn poll_delegated_repair_infrastructure_terminal(
        &mut self,
        run_key: &str,
        now: &Instant,
    ) -> Result<CodexPoll, ProductionCodexError> {
        self.store
            .commit_provider_final_model_calls(run_key)
            .map_err(map_store_error)?;
        let first_failure = self
            .runs
            .get(run_key)
            .ok_or_else(unknown_thread)?
            .record
            .terminal
            .is_none();
        if first_failure {
            {
                let run = self.runs.get_mut(run_key).ok_or_else(unknown_thread)?;
                run.record.last_activity_at = now.clone();
                run.record.terminal = Some(StoredTerminal::DelegatedRepairInfrastructureFailed);
                run.record.phase = StoredRunPhase::TerminalTracePending;
            }
            self.persist_run(run_key)?;
            self.quiesce_infrastructure_run(run_key, now).await;
        }
        self.poll_retained_terminal(run_key)?
            .ok_or_else(unavailable)
    }

    fn poll_batch_intent(
        &mut self,
        run_key: &str,
    ) -> Result<Option<CodexPoll>, ProductionCodexError> {
        let run = self.runs.get_mut(run_key).ok_or_else(unknown_thread)?;
        if run.batch_intent_emission == OneShotState::Consumed {
            return Ok(None);
        }
        let Some(intent) = run.record.batch_intent.as_ref() else {
            return Ok(None);
        };
        run.batch_intent_emission = OneShotState::Consumed;
        Ok(Some(CodexPoll::ChangeBatchProposed(Box::new(
            intent.event.clone(),
        ))))
    }

    async fn reconcile_format_repair(
        &mut self,
        run_key: &str,
        now: &Instant,
    ) -> Result<Option<CodexPoll>, ProductionCodexError> {
        let repair = {
            let run = self.runs.get_mut(run_key).ok_or_else(unknown_thread)?;
            if run.record.batch_intent.is_some()
                || run.record.terminal.is_some()
                || run.format_repair_reconciliation == OneShotState::Consumed
            {
                return Ok(None);
            }
            let Some(repair) = run.record.format_repair.clone() else {
                return Ok(None);
            };
            run.format_repair_reconciliation = OneShotState::Consumed;
            (
                run.record.kernel_session_id.clone(),
                repair.turn_id,
                turn_submission_options(&run.record),
            )
        };
        let reconciliation = self
            .reconcile_format_repair_turn(&repair.0, repair.1.clone(), repair.2)
            .await;
        let Ok(reconciliation) = reconciliation else {
            return self
                .poll_delegated_repair_infrastructure_terminal(run_key, now)
                .await
                .map(Some);
        };
        match reconciliation {
            ExactTurnReconciliation::Started { turn_id, .. } if turn_id == repair.1 => {
                let run = self.runs.get_mut(run_key).ok_or_else(unknown_thread)?;
                run.record.current_turn_id = Some(turn_id);
                run.record.last_activity_at = now.clone();
                if let Some(stored) = run.record.format_repair.as_mut() {
                    stored.submitted = true;
                }
                self.persist_run(run_key)?;
                Ok(Some(CodexPoll::Pending))
            }
            ExactTurnReconciliation::Completed(terminal) if terminal.turn_id == repair.1 => {
                {
                    let run = self.runs.get_mut(run_key).ok_or_else(unknown_thread)?;
                    run.record.current_turn_id = Some(terminal.turn_id.clone());
                    run.record
                        .last_agent_message
                        .clone_from(&terminal.last_agent_message);
                    run.record.last_tokens = terminal
                        .token_usage
                        .as_ref()
                        .map_or(0, |usage| usage.last_token_usage.total_tokens.max(0));
                    run.record.last_runtime_millis = terminal.duration_ms.unwrap_or(0).max(0);
                    run.record.last_activity_at = now.clone();
                }
                self.persist_run(run_key)?;
                self.store
                    .commit_provider_final_model_calls(run_key)
                    .map_err(map_store_error)?;
                self.accept_delegated_final_output(
                    run_key,
                    &terminal.turn_id,
                    terminal.last_agent_message.as_deref(),
                    now,
                )
                .map(Some)
            }
            ExactTurnReconciliation::Started { .. }
            | ExactTurnReconciliation::Completed(_)
            | ExactTurnReconciliation::Failed(_)
            | ExactTurnReconciliation::NotSubmitted { .. } => self
                .poll_delegated_repair_infrastructure_terminal(run_key, now)
                .await
                .map(Some),
        }
    }

    async fn reconcile_format_repair_turn(
        &mut self,
        session_id: &str,
        turn_id: String,
        options: TurnSubmissionOptions,
    ) -> Result<ExactTurnReconciliation, ()> {
        #[cfg(feature = "test-support")]
        if let Some(fault) = self.config.format_repair_faults.pop_front() {
            return match fault {
                ProductionFormatRepairFault::KernelRecoveryFailed => Err(()),
            };
        }
        self.kernel
            .reconcile_turn_exact(
                session_id,
                turn_id,
                FORMAT_REPAIR_PROMPT.to_owned(),
                options,
            )
            .await
            .map_err(|_| ())
    }

    fn accept_command_end(
        &mut self,
        run_key: &str,
        command_end: &codex_protocol::protocol::ExecCommandEndEvent,
        now: &Instant,
    ) -> Result<CodexPoll, ProductionCodexError> {
        if matches!(
            command_end.source,
            codex_protocol::protocol::ExecCommandSource::UserShell
        ) {
            return Ok(CodexPoll::Pending);
        }
        let command_duration = duration_millis(command_end.duration);
        self.record_performance_completion(
            run_key,
            PerformanceOperationKind::Tool,
            &command_end.call_id,
            now,
            Some(command_duration),
        )?;
        if validation_command(&command_end.command) {
            self.record_performance_start(
                run_key,
                PerformanceOperationKind::Validation,
                &command_end.call_id,
                now,
            )?;
            self.record_performance_completion(
                run_key,
                PerformanceOperationKind::Validation,
                &command_end.call_id,
                now,
                Some(command_duration),
            )?;
        }
        let Ok(stage) = self.retain_stage_command_end(run_key, command_end, now) else {
            return self.retain_stage_failure(run_key, now);
        };
        Ok(stage.map_or(CodexPoll::Pending, |stage| {
            CodexPoll::RuntimeTrace(Box::new(stage))
        }))
    }

    fn record_performance_start(
        &self,
        run_key: &str,
        kind: PerformanceOperationKind,
        operation_id: &str,
        now: &Instant,
    ) -> Result<(), ProductionCodexError> {
        self.store
            .record_performance_start(run_key, kind, operation_id, now)
            .map_err(map_store_error)
    }

    fn register_performance_run(&self, run_key: &str) -> Result<(), ProductionCodexError> {
        self.store
            .register_performance_run(
                run_key,
                self.config.execution_mode,
                self.config.observer_mode,
            )
            .map_err(map_store_error)
    }

    fn record_tool_start(
        &self,
        run_key: &str,
        operation_id: &str,
        now: &Instant,
    ) -> Result<bool, ProductionCodexError> {
        self.record_performance_start(run_key, PerformanceOperationKind::Tool, operation_id, now)?;
        Ok(true)
    }

    fn record_tool_completion(
        &self,
        run_key: &str,
        operation_id: &str,
        now: &Instant,
        duration: Option<i64>,
    ) -> Result<bool, ProductionCodexError> {
        self.record_performance_completion(
            run_key,
            PerformanceOperationKind::Tool,
            operation_id,
            now,
            duration,
        )?;
        Ok(true)
    }

    fn record_patch_start(
        &self,
        run_key: &str,
        operation_id: &str,
        now: &Instant,
    ) -> Result<bool, ProductionCodexError> {
        self.record_tool_start(run_key, operation_id, now)?;
        self.record_performance_start(run_key, PerformanceOperationKind::Patch, operation_id, now)?;
        Ok(true)
    }

    fn record_patch_completion(
        &self,
        run_key: &str,
        patch: &codex_protocol::protocol::PatchApplyEndEvent,
        now: &Instant,
    ) -> Result<bool, ProductionCodexError> {
        self.record_tool_completion(run_key, &patch.call_id, now, None)?;
        self.record_performance_completion(
            run_key,
            PerformanceOperationKind::Patch,
            &patch.call_id,
            now,
            None,
        )?;
        if patch.success {
            self.record_changed_files(run_key, patch.changes.keys())?;
        }
        Ok(true)
    }

    fn record_performance_completion(
        &self,
        run_key: &str,
        kind: PerformanceOperationKind,
        operation_id: &str,
        now: &Instant,
        duration: Option<i64>,
    ) -> Result<(), ProductionCodexError> {
        self.store
            .record_performance_completion(
                run_key,
                kind,
                operation_id,
                now,
                PerformanceOperationCompletion {
                    duration_millis: duration,
                    ..PerformanceOperationCompletion::default()
                },
            )
            .map_err(map_store_error)
    }

    fn record_changed_files<'path>(
        &self,
        run_key: &str,
        paths: impl Iterator<Item = &'path PathBuf>,
    ) -> Result<(), ProductionCodexError> {
        for path in paths {
            let mut digest = Sha256::new();
            digest.update(b"winwincode.performance-file.v1");
            digest.update(path.to_string_lossy().as_bytes());
            self.store
                .record_performance_changed_file(
                    run_key,
                    &Sha256Digest(format!("sha256:{:x}", digest.finalize())),
                )
                .map_err(map_store_error)?;
        }
        Ok(())
    }

    fn accept_error(
        &mut self,
        run_key: &str,
        now: &Instant,
    ) -> Result<CodexPoll, ProductionCodexError> {
        self.store
            .commit_provider_final_model_calls(run_key)
            .map_err(map_store_error)?;
        if self
            .runs
            .get(run_key)
            .is_some_and(|run| is_delegated_composer(&run.record))
        {
            if self
                .runs
                .get(run_key)
                .is_some_and(|run| is_active_format_repair(&run.record))
            {
                return self.retain_delegated_repair_infrastructure_failure(run_key, now);
            }
            return self.retain_delegated_inconclusive(run_key, now);
        }
        let run = self.runs.get_mut(run_key).ok_or_else(unknown_thread)?;
        run.record.last_activity_at = now.clone();
        run.record.terminal = Some(StoredTerminal::Failed);
        run.record.phase = StoredRunPhase::TerminalTracePending;
        self.persist_run(run_key)?;
        self.poll_retained_terminal(run_key)?
            .ok_or_else(unavailable)
    }

    fn retain_input_request(
        &self,
        run_key: &str,
        request: &RequestUserInputEvent,
    ) -> Result<Option<InputRequestMessage>, ProductionCodexError> {
        let run = self.runs.get(run_key).ok_or_else(unknown_thread)?;
        let [question] = request.questions.as_slice() else {
            return Err(unavailable());
        };
        if request.call_id.trim().is_empty() || question.id.trim().is_empty() {
            return Err(conflict());
        }
        let turn_id = non_empty(request.turn_id.clone())
            .or_else(|| run.record.current_turn_id.clone())
            .ok_or_else(conflict)?;
        let input_request_id = canonical_parts_id(
            "inp",
            b"winwincode-kernel-input.v1",
            &[
                run_key.as_bytes(),
                turn_id.as_bytes(),
                request.call_id.as_bytes(),
                question.id.as_bytes(),
            ],
        );
        let request_digest =
            private_payload_digest(b"winwincode.kernel-input-request.v1", request)?;
        let operation = StoredInputOperation {
            input_request_id: input_request_id.clone(),
            run_key: run_key.to_owned(),
            kernel_session_id: run.record.kernel_session_id.clone(),
            question_id: question.id.clone(),
            turn_id,
            request_digest,
            resolution_digest: None,
            state: StoredInputOperationState::Pending,
        };
        let already_resolved = if let Some(existing) = self
            .store
            .load_input_operation(&input_request_id)
            .map_err(map_store_error)?
        {
            // A resumed Core turn can replay the exact tool call after the
            // host response was durably accepted.  The response is already
            // queued in the new Session waiter, so the replayed event is an
            // internal recovery marker rather than a new host prompt.  Keep
            // all request fields immutable and suppress only this exact,
            // already-resolved operation; any changed request remains a
            // conflict.
            if existing.run_key != operation.run_key
                || existing.kernel_session_id != operation.kernel_session_id
                || existing.question_id != operation.question_id
                || existing.turn_id != operation.turn_id
                || existing.request_digest != operation.request_digest
            {
                return Err(conflict());
            }
            existing.state == StoredInputOperationState::Resolved
        } else {
            self.store
                .retain_input_operation(&operation)
                .map_err(map_store_error)?;
            false
        };
        if already_resolved {
            return Ok(None);
        }
        let authority = &run.binding.authority;
        let choices = question.options.as_ref().map(|options| {
            options
                .iter()
                .map(|option| InteractiveInputChoice {
                    label: option.label.clone(),
                    value: option.label.clone(),
                })
                .collect::<Vec<_>>()
        });
        let mode = if choices.as_ref().is_some_and(|choices| !choices.is_empty()) {
            InteractiveInputMode::SingleChoice
        } else {
            InteractiveInputMode::Text
        };
        Ok(Some(InputRequestMessage {
            allow_empty: false,
            choices,
            expires_at: authority.lease.expires_at.clone(),
            input_request_id: InputRequestId(input_request_id.clone()),
            kind: InputRequestMessageKind::InputRequest,
            lease: authority.lease.clone(),
            message_id: ExecutionMessageId(canonical_parts_id(
                "xmsg",
                b"winwincode-kernel-input-message.v1",
                &[input_request_id.as_bytes()],
            )),
            mode,
            prompt: question.question.clone(),
            request_id: RequestId(canonical_parts_id(
                "req",
                b"winwincode-kernel-input-request.v1",
                &[input_request_id.as_bytes()],
            )),
            schema_version: SchemaVersion::WinwincodeV1,
            sent_at: authority.lease.issued_at.clone(),
            session_identity: authority.session_identity.clone(),
            worker_session_id: authority.worker_session_id.clone(),
        }))
    }

    fn install_active_run(
        &mut self,
        run_key: String,
        mut record: StoredRun,
        binding: ModelRunBinding,
        kernel_live: bool,
        recovered: bool,
    ) -> Result<CodexThreadId, ProductionCodexError> {
        let thread_id = binding.canonical_thread_id.clone();
        let replay = load_runtime_messages(&mut self.store, &binding)?;
        for message in &replay {
            self.outbox
                .retain(&ExecutionPortMessage::RuntimeEventMessage(message.clone()))
                .map_err(map_store_error)?;
        }
        if record.phase == StoredRunPhase::TerminalTracePending
            && let Some(terminal_trace) = record.terminal_trace.as_mut()
            && let Some(message) = replay.iter().find(|message| {
                message.event.event_id == terminal_trace.event_id
                    && message.event.sequence == terminal_trace.sequence
            })
        {
            if message.event.summary
                != record
                    .terminal
                    .as_ref()
                    .map_or("", |terminal| terminal.trace_summary())
            {
                return Err(conflict());
            }
            terminal_trace.retained = true;
            record.phase = StoredRunPhase::Terminal;
        }
        let terminal_phase = matches!(
            record.phase,
            StoredRunPhase::Terminal | StoredRunPhase::OutcomeRetained
        );
        if terminal_phase
            && !record
                .terminal_trace
                .as_ref()
                .is_some_and(|trace| trace.retained)
        {
            return Err(conflict());
        }
        if record.terminal.is_none() && record.terminal_trace.is_some() {
            return Err(conflict());
        }
        if let Some(intent) = record.batch_intent.as_ref() {
            validate_stored_batch_intent(&record, &binding, intent)?;
        }
        self.store
            .save_run(&run_key, &record)
            .map_err(map_store_error)?;
        self.bridge
            .install_binding(binding.clone())
            .map_err(map_bridge_error)?;
        self.action_gate
            .install_binding(
                binding.clone(),
                is_delegated_composer(&record).then_some(record.workspace.as_path()),
            )
            .map_err(|_| unavailable())?;
        // Core may have emitted an approval event immediately before the
        // process stopped.  The durable operation is the source of truth for
        // the resumed action-gate queue; terminal runs must not resurrect a
        // stale approval after their outcome has been retained.
        if record.terminal.is_none() {
            for operation in self
                .store
                .list_pending_approval_operations(&run_key)
                .map_err(map_store_error)?
            {
                if operation.run_key != run_key
                    || operation.kernel_session_id != record.kernel_session_id
                {
                    return Err(conflict());
                }
                self.action_gate
                    .enqueue_message(ExecutionPortMessage::ApprovalRequestMessage(
                        approval_request_message(&operation, &binding.authority),
                    ))
                    .map_err(|_| unavailable())?;
            }
        }
        self.thread_to_run
            .insert(thread_id.0.clone(), run_key.clone());
        self.runs.insert(
            run_key,
            ActiveRun {
                record,
                binding,
                replay,
                kernel_live,
                recovered,
                batch_intent_emission: OneShotState::Ready,
                format_repair_reconciliation: OneShotState::Ready,
            },
        );
        Ok(thread_id)
    }

    fn retain_exec_approval_request(
        &self,
        run_key: &str,
        request: &ExecApprovalRequestEvent,
    ) -> Result<ApprovalRequestMessage, ProductionCodexError> {
        let operation_id = request
            .approval_id
            .clone()
            .unwrap_or_else(|| request.call_id.clone());
        let turn_id = non_empty(request.turn_id.clone());
        self.retain_approval_request(
            run_key,
            StoredApprovalOperationKind::Exec,
            operation_id,
            turn_id,
            private_payload_digest(b"winwincode.exec-approval-request.v1", request)?,
        )
    }

    fn retain_patch_approval_request(
        &self,
        run_key: &str,
        request: &ApplyPatchApprovalRequestEvent,
    ) -> Result<ApprovalRequestMessage, ProductionCodexError> {
        self.retain_approval_request(
            run_key,
            StoredApprovalOperationKind::Patch,
            request.call_id.clone(),
            non_empty(request.turn_id.clone()),
            private_payload_digest(b"winwincode.patch-approval-request.v1", request)?,
        )
    }

    fn retain_approval_request(
        &self,
        run_key: &str,
        operation_kind: StoredApprovalOperationKind,
        operation_id: String,
        turn_id: Option<String>,
        request_digest: String,
    ) -> Result<ApprovalRequestMessage, ProductionCodexError> {
        let run = self.runs.get(run_key).ok_or_else(unknown_thread)?;
        let kind = match operation_kind {
            StoredApprovalOperationKind::Exec => "exec",
            StoredApprovalOperationKind::Patch => "patch",
        };
        let approval_id = canonical_parts_id(
            "apr",
            b"winwincode-kernel-approval.v1",
            &[
                run_key.as_bytes(),
                kind.as_bytes(),
                turn_id.as_deref().unwrap_or("").as_bytes(),
                operation_id.as_bytes(),
            ],
        );
        let operation = StoredApprovalOperation {
            approval_id: approval_id.clone(),
            run_key: run_key.to_owned(),
            kernel_session_id: run.record.kernel_session_id.clone(),
            operation_kind,
            operation_id,
            turn_id,
            request_digest,
            resolution_digest: None,
            state: StoredApprovalOperationState::Pending,
        };
        if let Some(existing) = self
            .store
            .load_approval_operation(&approval_id)
            .map_err(map_store_error)?
        {
            if existing.approval_id != operation.approval_id
                || existing.run_key != operation.run_key
                || existing.operation_kind != operation.operation_kind
                || existing.operation_id != operation.operation_id
                || existing.turn_id != operation.turn_id
                || existing.request_digest != operation.request_digest
            {
                return Err(conflict());
            }
        } else {
            self.store
                .retain_approval_operation(&operation)
                .map_err(map_store_error)?;
        }
        let authority = &run.binding.authority;
        Ok(approval_request_message(&operation, authority))
    }

    async fn accept_approval_decision_exact(
        &mut self,
        decision: &ApprovalDecisionMessage,
        received_at: &Instant,
    ) -> Result<(), ProductionCodexError> {
        let operation = self
            .store
            .load_approval_operation(&decision.approval_id.0)
            .map_err(map_store_error)?
            .ok_or_else(conflict)?;
        let run = self
            .runs
            .get(&operation.run_key)
            .ok_or_else(unknown_thread)?;
        let authority = &run.binding.authority;
        if decision.lease != authority.lease
            || decision.worker_session_id != authority.worker_session_id
            || decision.session_identity != authority.session_identity
            || decision.sent_at != decision.decided_at
            || !canonical_instant(&decision.decided_at)
            || !canonical_instant(received_at)
            || !canonical_instant(&authority.lease.issued_at)
            || !canonical_instant(&authority.lease.expires_at)
            || decision.decided_at.0 < authority.lease.issued_at.0
            || decision.decided_at.0 >= authority.lease.expires_at.0
            || received_at.0 < authority.lease.issued_at.0
            || received_at.0 >= authority.lease.expires_at.0
            || !valid_prefixed_id(&decision.approval_id.0, "apr_")
            || !valid_prefixed_id(&decision.message_id.0, "xmsg_")
        {
            return Err(conflict());
        }
        self.action_gate
            .update_now(received_at)
            .map_err(|_| unavailable())?;
        let resolution_digest =
            private_payload_digest(b"winwincode.approval-decision.v1", decision)?;
        if operation.state == StoredApprovalOperationState::Resolved {
            self.store
                .resolve_approval_operation(
                    &operation.approval_id,
                    &operation.request_digest,
                    &resolution_digest,
                )
                .map_err(map_store_error)?;
            return Ok(());
        }
        let kernel_decision = match (&decision.decision, &decision.scope) {
            (ApprovalDecisionMessageDecision::Approved, ApprovalDecisionMessageScope::Once) => {
                ApprovalDecision::Approved
            }
            (
                ApprovalDecisionMessageDecision::Approved,
                ApprovalDecisionMessageScope::WorkerSession,
            ) => ApprovalDecision::ApprovedForSession,
            (ApprovalDecisionMessageDecision::Denied, _) => ApprovalDecision::Denied {
                rejection: "approval denied by Control Plane".to_owned(),
            },
            (
                ApprovalDecisionMessageDecision::Cancelled
                | ApprovalDecisionMessageDecision::Expired,
                _,
            ) => ApprovalDecision::Abort,
        };
        let response = ApprovalResponse {
            session_id: run.record.kernel_session_id.clone(),
            kind: match operation.operation_kind {
                StoredApprovalOperationKind::Exec => ApprovalKind::Exec,
                StoredApprovalOperationKind::Patch => ApprovalKind::Patch,
            },
            operation_id: operation.operation_id.clone(),
            turn_id: operation.turn_id.clone(),
            decision: kernel_decision,
        };
        self.kernel
            .resolve_approval(response)
            .await
            .map_err(|_| kernel_error())?;
        self.store
            .resolve_approval_operation(
                &operation.approval_id,
                &operation.request_digest,
                &resolution_digest,
            )
            .map_err(map_store_error)?;
        Ok(())
    }

    async fn recover_stored_kernel_session(
        &mut self,
        run_key: &str,
        record: &mut StoredRun,
        workspace: &Path,
        role_policy: Option<RoleSessionPolicy>,
    ) -> Result<bool, ProductionCodexError> {
        if record.terminal.is_some() {
            return Ok(false);
        }
        let rollout_path = record.rollout_path.clone().ok_or_else(|| {
            ProductionCodexError::new(
                ProductionCodexErrorKind::Restart,
                "durable Codex rollout is unavailable for exact restart",
            )
        })?;
        let options = session_options(&self.config, workspace, role_policy);
        let session = if rollout_path.is_file() {
            self.kernel
                .resume_session(rollout_path, options)
                .await
                .map_err(|_| kernel_error())?
        } else if matches!(
            record.phase,
            StoredRunPhase::Prepared | StoredRunPhase::SubmissionIntent
        ) {
            self.kernel
                .create_session(options)
                .await
                .map_err(|_| kernel_error())?
        } else {
            return Err(ProductionCodexError::new(
                ProductionCodexErrorKind::Restart,
                "durable Codex rollout is unavailable for exact restart",
            ));
        };
        let previous_kernel_session_id = record.kernel_session_id.clone();
        record.kernel_session_id = session.session_id;
        record.rollout_path = session.rollout_path.map(PathBuf::from);
        self.store
            .rebind_pending_approval_operations(
                run_key,
                &previous_kernel_session_id,
                &record.kernel_session_id,
            )
            .map_err(map_store_error)?;
        self.store
            .rebind_pending_input_operations(
                run_key,
                &previous_kernel_session_id,
                &record.kernel_session_id,
            )
            .map_err(map_store_error)?;
        self.store
            .save_run(run_key, record)
            .map_err(map_store_error)?;
        Ok(true)
    }
}

fn complete_reconciled_turn(
    adapter: &mut ProductionCodexAdapter,
    run_key: &str,
    turn_id: &str,
    final_message: Option<String>,
    last_tokens: i64,
    last_runtime_millis: i64,
) -> Result<(), ProductionCodexError> {
    let activity_at = {
        let run = adapter.runs.get_mut(run_key).ok_or_else(unknown_thread)?;
        run.record.current_turn_id = Some(turn_id.to_owned());
        run.record.last_agent_message.clone_from(&final_message);
        run.record.last_tokens = last_tokens;
        run.record.last_runtime_millis = last_runtime_millis;
        run.record.last_activity_at.clone()
    };
    adapter.record_performance_start(
        run_key,
        PerformanceOperationKind::Turn,
        turn_id,
        &activity_at,
    )?;
    adapter.persist_run(run_key)?;
    if adapter
        .runs
        .get(run_key)
        .is_some_and(|run| is_delegated_composer(&run.record))
    {
        adapter
            .store
            .commit_provider_final_model_calls(run_key)
            .map_err(map_store_error)?;
        let poll = adapter.accept_delegated_final_output(
            run_key,
            turn_id,
            final_message.as_deref(),
            &activity_at,
        )?;
        if matches!(poll, CodexPoll::ChangeBatchProposed(_)) {
            adapter
                .runs
                .get_mut(run_key)
                .ok_or_else(unknown_thread)?
                .batch_intent_emission = OneShotState::Ready;
        }
        return Ok(());
    }
    let Ok(_stage) = adapter.retain_stage_turn_completed(
        run_key,
        turn_id,
        final_message.as_deref(),
        false,
        &activity_at,
    ) else {
        let _ = adapter.retain_stage_failure(run_key, &activity_at)?;
        adapter
            .runs
            .get_mut(run_key)
            .ok_or_else(unknown_thread)?
            .recovered = false;
        return Ok(());
    };
    adapter
        .store
        .commit_provider_final_model_calls(run_key)
        .map_err(map_store_error)?;
    let run = adapter.runs.get_mut(run_key).ok_or_else(unknown_thread)?;
    run.record.terminal = Some(StoredTerminal::Completed {
        summary: "embedded Codex turn completed".to_owned(),
        final_message,
        artifacts: Vec::new(),
        usage: ExecutionOutcomeUsage {
            runtime_millis: run.record.last_runtime_millis,
            tokens: run.record.last_tokens,
            cost_microunits: 0,
        },
    });
    run.record.phase = StoredRunPhase::TerminalTracePending;
    adapter.persist_run(run_key)
}

impl CodexCoreAdapter for ProductionCodexAdapter {
    type Error = ProductionCodexError;

    fn observe_now(&mut self, now: &Instant) -> Result<(), Self::Error> {
        self.bridge
            .authority()
            .update_now(now)
            .map_err(map_bridge_error)?;
        self.action_gate
            .update_now(now)
            .map_err(|_| unavailable())?;
        self.kernel
            .enforce_private_permissions()
            .map_err(|_| kernel_error())
    }

    fn install_action_request_transport(&mut self, transport: ActionRequestTransport) {
        let outbox = self.outbox.clone();
        self.action_gate
            .install_request_transport(Arc::new(move |message| {
                let delivery = outbox.retain(&message).map_err(|_| ())?;
                let responses = transport(message);
                if responses.is_ok() {
                    outbox.record_sent(&delivery.delivery_id).map_err(|_| ())?;
                }
                responses
            }));
    }

    async fn ensure_thread(
        &mut self,
        start: CodexThreadStart<'_>,
    ) -> Result<CodexThreadId, Self::Error> {
        validate_start(start)?;
        let job_digest = stage_product_job_digest(start.job).map_err(|_| invalid_job())?;
        let role_policy = configured_role_session_policy(&self.config, start.job)?;
        let workspace = canonical_workspace(start.workspace)?;
        let run_key = start
            .run_key
            .canonical_digest()
            .map_err(|_| unavailable())?
            .0;
        self.register_performance_run(&run_key)?;
        if let Some(run) = self.runs.get(&run_key) {
            if run.record.job_digest == job_digest
                && run.record.workspace == workspace
                && run.record.workspace_revision == *start.workspace_revision
                && run.binding.authority.lease == *start.lease
                && run.binding.authority.worker_session_id == *start.worker_session_id
            {
                return Ok(run.binding.canonical_thread_id.clone());
            }
            return Err(conflict());
        }
        let thread_id = start
            .run_key
            .canonical_thread_id()
            .map_err(|_| unavailable())?;
        let session_identity = session_identity(start, thread_id.clone());
        let authority = ModelLeaseAuthority {
            lease: start.lease.clone(),
            worker_session_id: start.worker_session_id.clone(),
            session_identity,
        };
        if let Some(mut record) = load_stored_run(&self.store, &run_key)? {
            if record.canonical_thread_id != thread_id
                || record.job != *start.job
                || record.job_digest != job_digest
                || record.workspace != workspace
                || record.workspace_revision != *start.workspace_revision
                || record.role_policy != role_policy
            {
                return Err(conflict());
            }
            let kernel_live = match self
                .recover_stored_kernel_session(
                    &run_key,
                    &mut record,
                    &workspace,
                    role_policy.clone(),
                )
                .await
            {
                Ok(kernel_live) => kernel_live,
                Err(error) => return Err(error),
            };
            let binding = ModelRunBinding {
                run_key: run_key.clone(),
                canonical_thread_id: thread_id,
                kernel_session_id: record.kernel_session_id.clone(),
                authority,
                opened_at: record.last_activity_at.clone(),
            };
            let result = self.install_active_run(run_key, record, binding, kernel_live, true);
            return result;
        }
        let session = self
            .kernel
            .create_session(session_options(
                &self.config,
                &workspace,
                role_policy.clone(),
            ))
            .await
            .map_err(|_| kernel_error())?;
        let record = prepared_stored_run(
            &start,
            thread_id.clone(),
            job_digest,
            workspace,
            role_policy,
            session.session_id.clone(),
            session.rollout_path.map(PathBuf::from),
        );
        self.store
            .save_run(&run_key, &record)
            .map_err(map_store_error)?;
        let binding = ModelRunBinding {
            run_key: run_key.clone(),
            canonical_thread_id: thread_id,
            kernel_session_id: session.session_id,
            authority,
            opened_at: start.lease.issued_at.clone(),
        };
        self.install_active_run(run_key, record, binding, true, false)
    }

    async fn submit_turn(
        &mut self,
        thread_id: &CodexThreadId,
        goal: &str,
    ) -> Result<(), Self::Error> {
        let run_key = self.run_key_for_thread(thread_id)?.to_owned();
        // The Worker normally supplies this prompt, but the adapter remains
        // the final boundary for the sealed stage input.  Comparing against
        // the Job persisted by `ensure_thread` prevents a caller from
        // changing the role prompt or smuggling mutable stage state into a
        // retry after the session has been opened.
        let expected_goal = {
            let run = self.runs.get(&run_key).ok_or_else(unknown_thread)?;
            stage_product_prompt(&run.record.job).map_err(|_| invalid_job())?
        };
        if goal != expected_goal {
            return Err(conflict());
        }
        let submission_options = {
            let run = self.runs.get(&run_key).ok_or_else(unknown_thread)?;
            turn_submission_options(&run.record)
        };
        let submission_digest = submission_input_digest(goal, &submission_options)?;
        let (settled, submission_id) = {
            let run = self.runs.get_mut(&run_key).ok_or_else(unknown_thread)?;
            match &run.record.submission_digest {
                Some(existing) if existing != &submission_digest => return Err(conflict()),
                Some(_) => {}
                None => run.record.submission_digest = Some(submission_digest),
            }
            if run.record.terminal.is_some()
                || run.record.batch_intent.is_some()
                || run.record.format_repair.is_some()
            {
                (true, run.record.submission_id.clone())
            } else {
                if run.record.phase != StoredRunPhase::Prepared && !run.recovered {
                    return Err(conflict());
                }
                if run.record.phase == StoredRunPhase::Prepared {
                    run.record.phase = StoredRunPhase::SubmissionIntent;
                }
                (false, run.record.submission_id.clone())
            }
        };
        self.persist_run(&run_key)?;
        if settled {
            return Ok(());
        }
        #[cfg(feature = "test-support")]
        if self.config.submission_faults.pop_front()
            == Some(ProductionSubmissionFault::AfterIntentBeforeKernel)
        {
            return Err(ProductionCodexError::new(
                ProductionCodexErrorKind::Restart,
                "test submission stopped before embedded Kernel call",
            ));
        }
        let session = self.session_for_thread(thread_id)?;
        let reconciliation = self
            .kernel
            .reconcile_turn_exact(
                &session,
                submission_id.clone(),
                goal.to_owned(),
                submission_options,
            )
            .await;
        let Ok(submission) = reconciliation else {
            self.retain_submission_failure(&run_key).await?;
            return Err(kernel_error());
        };
        match submission {
            ExactTurnReconciliation::Started { turn_id, .. } if turn_id == submission_id => {}
            ExactTurnReconciliation::Completed(terminal) => {
                // Core reconstructs the terminal token snapshot from the
                // durable rollout, so restart reconciliation preserves usage
                // even when the adapter never observed the TokenCount event.
                let last_tokens = terminal
                    .token_usage
                    .as_ref()
                    .map_or(0, |usage| usage.last_token_usage.total_tokens.max(0));
                complete_reconciled_turn(
                    self,
                    &run_key,
                    &terminal.turn_id,
                    terminal.last_agent_message,
                    last_tokens,
                    terminal.duration_ms.unwrap_or(0).max(0),
                )?;
            }
            ExactTurnReconciliation::Failed(_) => {
                self.store
                    .commit_provider_final_model_calls(&run_key)
                    .map_err(map_store_error)?;
                let run = self.runs.get_mut(&run_key).ok_or_else(unknown_thread)?;
                run.record.terminal = Some(StoredTerminal::Failed);
                run.record.phase = StoredRunPhase::TerminalTracePending;
                self.persist_run(&run_key)?;
            }
            ExactTurnReconciliation::Started { .. }
            | ExactTurnReconciliation::NotSubmitted { .. } => {
                self.retain_submission_failure(&run_key).await?;
                return Err(kernel_error());
            }
        }
        self.runs
            .get_mut(&run_key)
            .ok_or_else(unknown_thread)?
            .recovered = false;
        Ok(())
    }

    async fn poll(
        &mut self,
        thread_id: &CodexThreadId,
        now: &Instant,
    ) -> Result<CodexPoll, Self::Error> {
        self.bridge
            .authority()
            .update_now(now)
            .map_err(map_bridge_error)?;
        self.action_gate
            .update_now(now)
            .map_err(|_| unavailable())?;
        let run_key = self.run_key_for_thread(thread_id)?.to_owned();
        if let Some(message) = self
            .runs
            .get_mut(&run_key)
            .and_then(|run| run.replay.pop_front())
        {
            return Ok(CodexPoll::RuntimeTrace(Box::new(message)));
        }
        if let Some(intent) = self.poll_batch_intent(&run_key)? {
            return Ok(intent);
        }
        if self
            .runs
            .get(&run_key)
            .is_some_and(|run| run.record.batch_intent.is_some())
        {
            return Ok(CodexPoll::Pending);
        }
        if let Some(repair) = self.reconcile_format_repair(&run_key, now).await? {
            return Ok(repair);
        }
        if let Some(terminal) = self.poll_retained_terminal(&run_key)? {
            return Ok(terminal);
        }
        let session = self.session_for_thread(thread_id)?;
        let Ok(event) = self.next_kernel_event(&session).await else {
            return self.poll_infrastructure_terminal(&run_key, now).await;
        };
        let EventPoll::Event(event) = event else {
            return match event {
                EventPoll::Timeout => Ok(CodexPoll::Pending),
                EventPoll::Closed => self.poll_infrastructure_terminal(&run_key, now).await,
                EventPoll::Event(_) => unreachable!(),
            };
        };
        let Ok(event) = decode_kernel_event(&event.payload_json) else {
            return self.poll_infrastructure_terminal(&run_key, now).await;
        };
        let is_error_event = matches!(&event.msg, CodexEventMsg::Error(_));
        let result = self.accept_polled_event(&run_key, event, now);
        if is_error_event && result.is_ok() {
            // A terminal ErrorEvent can arrive before the first TurnStarted
            // frame.  Quiesce the bridge before WorkerMain flushes queued
            // model frames so that this pre-start fault has no Provider side
            // effect and cannot leave a live Core session behind.
            self.quiesce_infrastructure_run(&run_key, now).await;
        }
        result
    }

    async fn accept_model_chunk(
        &mut self,
        chunk: &ModelChunkMessage,
        received_at: &Instant,
    ) -> Result<(), Self::Error> {
        let disposition = self
            .bridge
            .accept_chunk(chunk, received_at)
            .await
            .map_err(map_bridge_error)?;
        self.last_duplicate_model_chunk_sequence =
            matches!(disposition, ModelChunkDisposition::Duplicate { .. })
                .then_some(chunk.sequence.0);
        self.bridge
            .authority()
            .update_now(received_at)
            .map_err(map_bridge_error)?;
        let binding = self
            .bridge
            .binding_for_thread(&chunk.session_identity.codex_thread_id)
            .map_err(map_bridge_error)?;
        if let Some(binding) = binding {
            self.action_gate
                .install_child_binding(binding.clone())
                .map_err(|_| unavailable())?;
            let run_key = binding.run_key;
            let run = self.runs.get_mut(&run_key).ok_or_else(unknown_thread)?;
            run.record.last_activity_at = received_at.clone();
            self.persist_run(&run_key)?;
        }
        Ok(())
    }

    async fn accept_action_receipt(
        &mut self,
        receipt: &winwincode_execution_port::generated::ActionEnforcementReceiptMessage,
        received_at: &Instant,
    ) -> Result<(), Self::Error> {
        if let Err(error) = self.action_gate.accept_receipt(receipt, received_at)
            && !matches!(error, ActionBridgeError::Consumed)
        {
            return Err(ProductionCodexError::new(
                ProductionCodexErrorKind::Authority,
                "embedded Codex action receipt was rejected",
            ));
        }
        self.bridge
            .authority()
            .update_now(received_at)
            .map_err(map_bridge_error)
    }

    async fn accept_approval_decision(
        &mut self,
        decision: &ApprovalDecisionMessage,
        received_at: &Instant,
    ) -> Result<(), Self::Error> {
        self.accept_approval_decision_exact(decision, received_at)
            .await?;
        self.bridge
            .authority()
            .update_now(received_at)
            .map_err(map_bridge_error)
    }

    async fn accept_input_response(
        &mut self,
        response: &InputResponseMessage,
        received_at: &Instant,
    ) -> Result<(), Self::Error> {
        let operation = self
            .store
            .load_input_operation(&response.input_request_id.0)
            .map_err(map_store_error)?
            .ok_or_else(conflict)?;
        let run = self
            .runs
            .get(&operation.run_key)
            .ok_or_else(unknown_thread)?;
        let authority = &run.binding.authority;
        if response.lease != authority.lease
            || response.worker_session_id != authority.worker_session_id
            || response.session_identity != authority.session_identity
            || response.sent_at != response.responded_at
            || !canonical_instant(&response.responded_at)
            || !canonical_instant(received_at)
            || !canonical_instant(&authority.lease.issued_at)
            || !canonical_instant(&authority.lease.expires_at)
            || response.responded_at.0 < authority.lease.issued_at.0
            || response.responded_at.0 >= authority.lease.expires_at.0
            || received_at.0 < authority.lease.issued_at.0
            || received_at.0 >= authority.lease.expires_at.0
            || !valid_prefixed_id(&response.input_request_id.0, "inp_")
            || !valid_prefixed_id(&response.message_id.0, "xmsg_")
        {
            return Err(conflict());
        }
        let value = match response.status {
            InputResponseMessageStatus::Provided => {
                let value = response.value.as_ref().ok_or_else(conflict)?;
                if value.value.trim().is_empty() {
                    return Err(conflict());
                }
                Some(value.value.clone())
            }
            InputResponseMessageStatus::Cancelled | InputResponseMessageStatus::Expired => {
                if response.value.is_some() {
                    return Err(conflict());
                }
                None
            }
        };
        // Advance the shared trusted clock only after the response has passed
        // all identity, lease, shape, and value checks.  An invalid response
        // must not be able to push a later valid operation past its lease.
        self.bridge
            .authority()
            .update_now(received_at)
            .map_err(map_bridge_error)?;
        let resolution_digest = private_payload_digest(b"winwincode.input-response.v1", response)?;
        if operation.state == StoredInputOperationState::Resolved {
            self.store
                .resolve_input_operation(
                    &operation.input_request_id,
                    &operation.request_digest,
                    &resolution_digest,
                )
                .map_err(map_store_error)?;
            return Ok(());
        }
        let mut answers = HashMap::new();
        answers.insert(
            operation.question_id.clone(),
            RequestUserInputAnswer {
                answers: value.into_iter().collect(),
            },
        );
        let resolution = self
            .kernel
            .resolve_user_input(
                &operation.kernel_session_id,
                operation.turn_id.clone(),
                RequestUserInputResponse { answers },
            )
            .await;
        resolution.map_err(|_| kernel_error())?;
        self.store
            .resolve_input_operation(
                &operation.input_request_id,
                &operation.request_digest,
                &resolution_digest,
            )
            .map_err(map_store_error)?;
        let run = self
            .runs
            .get_mut(&operation.run_key)
            .ok_or_else(unknown_thread)?;
        run.record.last_activity_at = received_at.clone();
        self.persist_run(&operation.run_key)?;
        Ok(())
    }

    fn retain_execution_delivery(
        &mut self,
        message: &ExecutionPortMessage,
    ) -> Result<DurableExecutionDelivery, Self::Error> {
        match self.outbox.retain(message) {
            Ok(delivery) => Ok(delivery),
            Err(error) => Err(map_store_error(error)),
        }
    }

    fn pending_execution_deliveries(
        &mut self,
    ) -> Result<Vec<DurableExecutionDelivery>, Self::Error> {
        self.outbox.pending().map_err(map_store_error)
    }

    fn recovered_message_sequence(&mut self) -> Result<u64, Self::Error> {
        self.outbox
            .highest_numeric_message_sequence()
            .map_err(map_store_error)
    }

    fn recovered_heartbeat_sequence(
        &mut self,
        worker_id: &WorkerId,
        worker_instance_id: &WorkerInstanceId,
    ) -> Result<i64, Self::Error> {
        self.outbox
            .heartbeat_sequence_highwater(worker_id, worker_instance_id)
            .map_err(map_store_error)
    }

    fn record_execution_delivery_sent(&mut self, delivery_id: &str) -> Result<(), Self::Error> {
        self.outbox
            .record_sent(delivery_id)
            .map_err(map_store_error)
    }

    fn accept_execution_delivery_ack(
        &mut self,
        acknowledgement: &ExecutionPortMessage,
    ) -> Result<(), Self::Error> {
        if let ExecutionPortMessage::ModelChunkMessage(chunk) = acknowledgement
            && chunk.sequence.0 == 1
            && self.last_duplicate_model_chunk_sequence.take() == Some(chunk.sequence.0)
        {
            // The bridge has already validated and delivered this exact
            // duplicate through the durable model cursor.  Compact the open
            // when it is still present (for a crash between chunk delivery
            // and the first acknowledgement), while treating an already
            // compacted open as the expected idempotent replay.
            return match self.outbox.acknowledge_response(acknowledgement) {
                Ok(()) | Err(AdapterStoreError::Conflict) => Ok(()),
                Err(error) => Err(map_store_error(error)),
            };
        }
        if let ExecutionPortMessage::RuntimeAckMessage(acknowledgement) = acknowledgement {
            let receipt = self
                .installation
                .runtime_trace_outbox
                .acknowledge(&mut self.store, &self.bridge.authority(), acknowledgement)
                .map_err(|_| unavailable())?;
            self.outbox
                .apply_runtime_ack(acknowledgement, &receipt)
                .map(|_| ())
                .map_err(map_store_error)
        } else {
            self.outbox
                .acknowledge_response(acknowledgement)
                .map_err(map_store_error)
        }
    }

    fn retain_candidate_artifact(
        &mut self,
        upload: &CandidateArtifactUpload,
    ) -> Result<RetainedCandidateArtifact, Self::Error> {
        self.candidate_artifacts
            .retain(upload)
            .map_err(map_store_error)
    }

    fn accept_candidate_artifact_ack(
        &mut self,
        acknowledgement: &ArtifactAckMessage,
    ) -> Result<CandidateArtifactAckOutcome, Self::Error> {
        self.candidate_artifacts
            .apply_ack(acknowledgement)
            .map_err(map_store_error)
    }

    fn accepted_candidate_artifact(
        &mut self,
        authority: &CandidateArtifactAuthority,
    ) -> Result<Option<ArtifactReference>, Self::Error> {
        self.candidate_artifacts
            .accepted_reference(authority)
            .map_err(map_store_error)
    }

    fn begin_candidate_artifact_cancel(
        &mut self,
        authority: &CandidateArtifactAuthority,
    ) -> Result<(), Self::Error> {
        self.candidate_artifacts
            .request_cancel(authority)
            .map_err(map_store_error)
    }

    fn candidate_artifact_delivery_allowed(
        &mut self,
        message: &ExecutionPortMessage,
    ) -> Result<bool, Self::Error> {
        self.candidate_artifacts
            .delivery_allowed(message)
            .map_err(map_store_error)
    }

    fn cancel_candidate_artifact(
        &mut self,
        authority: &CandidateArtifactAuthority,
    ) -> Result<(), Self::Error> {
        self.candidate_artifacts
            .cancel(authority)
            .map_err(map_store_error)
    }

    fn replay_execution_deliveries(
        &mut self,
        request: &RuntimeReplayRequestMessage,
    ) -> Result<Vec<DurableExecutionDelivery>, Self::Error> {
        let batch = self
            .installation
            .runtime_trace_outbox
            .resume(&mut self.store, &self.bridge.authority(), request)
            .map_err(|_| unavailable())?;
        self.outbox
            .requeue_runtime_events(&batch.events)
            .map_err(map_store_error)
    }

    fn retain_job_outcome(
        &mut self,
        thread_id: &CodexThreadId,
        outcome: &JobOutcomeMessage,
    ) -> Result<DurableExecutionDelivery, Self::Error> {
        let run_key = self.run_key_for_thread(thread_id)?.to_owned();
        let run = self.runs.get(&run_key).ok_or_else(unknown_thread)?;
        if run.record.terminal.is_none()
            || outcome.lease != run.binding.authority.lease
            || outcome.worker_session_id != run.binding.authority.worker_session_id
            || outcome.session_identity != run.binding.authority.session_identity
            || outcome.outcome.codex_thread_id.as_ref() != Some(thread_id)
        {
            return Err(conflict());
        }
        // The Control Plane may compact the acknowledged outcome row. Keep
        // its original transport identity in the durable run so a later
        // restart can reconstruct the same terminal frame byte-for-byte
        // rather than allocating a new Worker message sequence.
        let canonical_message_id = run
            .record
            .terminal_message_id
            .clone()
            .unwrap_or_else(|| outcome.message_id.clone());
        let mut finalized = run.record.clone();
        finalized.phase = StoredRunPhase::OutcomeRetained;
        finalized.terminal_message_id = Some(canonical_message_id.clone());
        let mut canonical_outcome = outcome.clone();
        canonical_outcome.message_id = canonical_message_id;
        let message = ExecutionPortMessage::JobOutcomeMessage(canonical_outcome);
        let delivery = self
            .store
            .transaction(|transaction| {
                AdapterStore::save_run_in_transaction(transaction, &run_key, &finalized)?;
                ExecutionOutbox::retain_in_transaction(transaction, &message)
            })
            .map_err(map_store_error)?;
        self.runs
            .get_mut(&run_key)
            .ok_or_else(unknown_thread)?
            .record = finalized;
        Ok(delivery)
    }

    fn take_execution_messages(&mut self) -> Result<Vec<ExecutionPortMessage>, Self::Error> {
        let mut messages = self.bridge.take_messages().map_err(map_bridge_error)?;
        messages.extend(
            self.action_gate
                .take_messages()
                .map_err(|_| unavailable())?,
        );
        messages
            .iter()
            .map(|message| {
                self.outbox
                    .retain(message)
                    .map(|delivery| delivery.message)
                    .map_err(map_store_error)
            })
            .collect()
    }

    async fn interrupt(
        &mut self,
        thread_id: &CodexThreadId,
        interrupted_at: &Instant,
    ) -> Result<(), Self::Error> {
        let run_key = self.run_key_for_thread(thread_id)?.to_owned();
        let session = self.session_for_thread(thread_id)?;
        // Advance the action-gate generation before awaiting Core.  A receipt
        // that arrives while interrupt is in flight must not authorize a
        // side effect after the caller has cancelled this session.
        self.action_gate
            .cancel_session(&session)
            .map_err(|_| unavailable())?;
        self.kernel
            .interrupt(&session)
            .await
            .map_err(|_| kernel_error())?;
        self.bridge
            .cancel_thread(thread_id, interrupted_at)
            .await
            .map_err(map_bridge_error)?;
        {
            let run = self.runs.get_mut(&run_key).ok_or_else(unknown_thread)?;
            run.record.last_activity_at = interrupted_at.clone();
        }
        {
            let run = self.runs.get_mut(&run_key).ok_or_else(unknown_thread)?;
            run.record.terminal = Some(StoredTerminal::Cancelled);
            run.record.phase = StoredRunPhase::TerminalTracePending;
        }
        self.persist_run(&run_key)?;
        let _ = self.retain_performance_baseline_trace(&run_key)?;
        let _ = self.retain_terminal_trace(&run_key, "embedded Codex turn cancelled")?;
        Ok(())
    }

    async fn close_thread(&mut self, thread_id: &CodexThreadId) -> Result<(), Self::Error> {
        let run_key = self.run_key_for_thread(thread_id)?.to_owned();
        let run = self.runs.remove(&run_key).ok_or_else(unknown_thread)?;
        self.thread_to_run.remove(&thread_id.0);
        self.action_gate
            .cancel_session(&run.record.kernel_session_id)
            .map_err(|_| unavailable())?;
        if run.kernel_live {
            self.kernel
                .close_session(&run.record.kernel_session_id)
                .await
                .map_err(|_| kernel_error())?;
        }
        let _ = self
            .bridge
            .discard_messages_for_thread(&run.binding.canonical_thread_id);
        // Closing embedded Core is independent from draining the durable
        // Worker outbox. Keep the exact lease/session lineage available so a
        // final RuntimeEvent or JobOutcome ACK arriving after this close is
        // still validated against the original run.
        self.bridge
            .detach_binding(&run.binding)
            .map_err(map_bridge_error)?;
        self.action_gate
            .remove_binding(&run.binding)
            .map_err(|_| unavailable())
    }

    async fn shutdown(&mut self) -> Result<(), Self::Error> {
        let sessions = self
            .runs
            .values()
            .map(|run| run.record.kernel_session_id.clone())
            .collect::<Vec<_>>();
        for session in sessions {
            self.action_gate
                .cancel_session(&session)
                .map_err(|_| unavailable())?;
        }
        self.kernel.shutdown().await.map_err(|_| kernel_error())?;
        self.runs.clear();
        self.thread_to_run.clear();
        Ok(())
    }
}

fn approval_request_message(
    operation: &StoredApprovalOperation,
    authority: &ModelLeaseAuthority,
) -> ApprovalRequestMessage {
    let (category, summary) = match operation.operation_kind {
        StoredApprovalOperationKind::Exec => (
            ApprovalActionCategory::Shell,
            "Approve embedded shell execution.",
        ),
        StoredApprovalOperationKind::Patch => (
            ApprovalActionCategory::FilesystemWrite,
            "Approve embedded file changes.",
        ),
    };
    let approval_id = &operation.approval_id;
    ApprovalRequestMessage {
        action: ApprovalAction {
            category,
            details: None,
            summary: summary.to_owned(),
        },
        approval_id: ApprovalId(approval_id.clone()),
        expires_at: authority.lease.expires_at.clone(),
        kind: ApprovalRequestMessageKind::ApprovalRequest,
        lease: authority.lease.clone(),
        message_id: ExecutionMessageId(canonical_parts_id(
            "xmsg",
            b"winwincode-kernel-approval-message.v1",
            &[approval_id.as_bytes()],
        )),
        request_id: RequestId(canonical_parts_id(
            "req",
            b"winwincode-kernel-approval-request.v1",
            &[approval_id.as_bytes()],
        )),
        schema_version: SchemaVersion::WinwincodeV1,
        sent_at: authority.lease.issued_at.clone(),
        session_identity: authority.session_identity.clone(),
        worker_session_id: authority.worker_session_id.clone(),
    }
}

struct ActiveRun {
    record: StoredRun,
    binding: ModelRunBinding,
    replay: VecDeque<RuntimeEventMessage>,
    kernel_live: bool,
    recovered: bool,
    batch_intent_emission: OneShotState,
    format_repair_reconciliation: OneShotState,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum OneShotState {
    Ready,
    Consumed,
}

fn prepared_stored_run(
    start: &CodexThreadStart<'_>,
    canonical_thread_id: CodexThreadId,
    job_digest: Sha256Digest,
    workspace: PathBuf,
    role_policy: Option<RoleSessionPolicy>,
    kernel_session_id: String,
    rollout_path: Option<PathBuf>,
) -> StoredRun {
    StoredRun {
        job: start.job.clone(),
        workspace_revision: start.workspace_revision.clone(),
        canonical_thread_id,
        job_digest,
        workspace,
        role_policy,
        kernel_session_id,
        rollout_path,
        submission_id: Uuid::now_v7().to_string(),
        submission_digest: None,
        phase: StoredRunPhase::Prepared,
        last_tokens: 0,
        last_runtime_millis: 0,
        last_activity_at: start.lease.issued_at.clone(),
        terminal: None,
        terminal_trace: None,
        current_turn_id: None,
        last_agent_message: None,
        stage_product_sources: Vec::new(),
        batch_intent: None,
        format_repair: None,
        terminal_message_id: None,
    }
}

fn load_stored_run(
    store: &AdapterStore,
    run_key: &str,
) -> Result<Option<StoredRun>, ProductionCodexError> {
    store.load_run(run_key).map_err(map_store_error)
}

/// Consumes the pre-v2 role-policy shape exactly once while the durable store
/// is opening. After this transaction commits, every runtime load parses only
/// [`StoredRun`] and therefore only the canonical v2 policy.
fn migrate_stored_run_role_policies_v1_to_v2(
    store: &AdapterStore,
) -> Result<(), ProductionCodexError> {
    store
        .migrate_run_records_once(ROLE_POLICY_V2_MIGRATION, |_, bytes| {
            let mut value: Value =
                serde_json::from_slice(bytes).map_err(|_| AdapterStoreError::Corrupt)?;
            let object = value.as_object_mut().ok_or(AdapterStoreError::Corrupt)?;
            let job: ExecutionJob = serde_json::from_value(
                object
                    .get("job")
                    .cloned()
                    .ok_or(AdapterStoreError::Corrupt)?,
            )
            .map_err(|_| AdapterStoreError::Corrupt)?;
            let migration =
                migrate_persisted_role_session_policy_v1(&job, object.get("rolePolicy"))
                    .map_err(|_| AdapterStoreError::Corrupt)?;
            if migration.migrated {
                object.insert(
                    "rolePolicy".to_owned(),
                    serde_json::to_value(&migration.policy)
                        .map_err(|_| AdapterStoreError::Corrupt)?,
                );
            }
            let canonical: StoredRun =
                serde_json::from_value(value).map_err(|_| AdapterStoreError::Corrupt)?;
            migration
                .migrated
                .then(|| serde_json::to_vec(&canonical).map_err(|_| AdapterStoreError::Corrupt))
                .transpose()
        })
        .map(|_| ())
        .map_err(map_store_error)
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct StoredRun {
    job: ExecutionJob,
    workspace_revision: WorkspaceRevision,
    canonical_thread_id: CodexThreadId,
    job_digest: Sha256Digest,
    workspace: PathBuf,
    role_policy: Option<RoleSessionPolicy>,
    kernel_session_id: String,
    rollout_path: Option<PathBuf>,
    submission_id: String,
    submission_digest: Option<Sha256Digest>,
    phase: StoredRunPhase,
    last_tokens: i64,
    last_runtime_millis: i64,
    last_activity_at: Instant,
    terminal: Option<StoredTerminal>,
    terminal_trace: Option<StoredTerminalTrace>,
    #[serde(default)]
    current_turn_id: Option<String>,
    #[serde(default)]
    last_agent_message: Option<String>,
    #[serde(default)]
    stage_product_sources: Vec<String>,
    /// Durable single-writer intent emitted by a delegated Composer instead
    /// of terminalizing the execution Job.
    #[serde(default)]
    batch_intent: Option<StoredBatchIntent>,
    /// At most one schema-preserving repair turn for malformed delegated
    /// output. The repair prompt never authorizes workspace side effects.
    #[serde(default)]
    format_repair: Option<StoredFormatRepair>,
    /// Original terminal `JobOutcome` message id, retained across CP ACK
    /// compaction so exact terminal replay does not allocate a new id.
    #[serde(default)]
    terminal_message_id: Option<ExecutionMessageId>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct StoredBatchIntent {
    event: ChangeBatchProposalEvent,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct StoredFormatRepair {
    turn_id: String,
    submitted: bool,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct StoredTerminalTrace {
    event_id: ExecutionEventId,
    sequence: ExecutionSequence,
    retained: bool,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
enum StoredRunPhase {
    Prepared,
    SubmissionIntent,
    RuntimeStarted,
    TerminalTracePending,
    Terminal,
    OutcomeRetained,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
enum StoredTerminal {
    Completed {
        summary: String,
        final_message: Option<String>,
        artifacts: Vec<ArtifactReference>,
        usage: ExecutionOutcomeUsage,
    },
    Failed,
    DelegatedInconclusive,
    DelegatedRepairInfrastructureFailed,
    Cancelled,
    InfrastructureFailed,
}

impl StoredTerminal {
    fn trace_summary(&self) -> &'static str {
        match self {
            Self::Completed { .. } => "embedded Codex turn stopped",
            Self::Failed => "embedded Codex turn failed",
            Self::DelegatedInconclusive => "delegated ChangeBatch proposal was inconclusive",
            Self::DelegatedRepairInfrastructureFailed => {
                "delegated format-repair infrastructure failure"
            }
            Self::Cancelled => "embedded Codex turn cancelled",
            Self::InfrastructureFailed => "embedded Codex infrastructure failure",
        }
    }

    fn into_poll(self) -> Result<CodexPoll, ProductionCodexError> {
        match self {
            Self::Completed {
                summary,
                final_message: _,
                artifacts,
                usage,
            } => Ok(CodexPoll::Completed(CodexTurnCompletion {
                summary: SecretSafeTraceSummary::new(summary).map_err(|_| unavailable())?,
                artifacts,
                usage,
            })),
            Self::Failed => Ok(CodexPoll::Failed(
                SecretSafeTraceSummary::new("embedded Codex turn failed")
                    .map_err(|_| unavailable())?,
            )),
            Self::DelegatedInconclusive => Ok(CodexPoll::Inconclusive(
                SecretSafeTraceSummary::new("delegated ChangeBatch proposal was inconclusive")
                    .map_err(|_| unavailable())?,
            )),
            Self::DelegatedRepairInfrastructureFailed => Ok(CodexPoll::InfrastructureFailed(
                SecretSafeTraceSummary::new("delegated format-repair infrastructure failure")
                    .map_err(|_| unavailable())?,
            )),
            Self::Cancelled => Ok(CodexPoll::Cancelled(
                SecretSafeTraceSummary::new("embedded Codex turn cancelled")
                    .map_err(|_| unavailable())?,
            )),
            Self::InfrastructureFailed => Ok(CodexPoll::InfrastructureFailed(
                SecretSafeTraceSummary::new("embedded Codex infrastructure failure")
                    .map_err(|_| unavailable())?,
            )),
        }
    }
}

fn validate_start(start: CodexThreadStart<'_>) -> Result<(), ProductionCodexError> {
    if start.run_key.job_id != start.job.job_id
        || start.run_key.attempt != start.job.attempt
        || start.run_key.job_id != start.lease.job_id
        || start.run_key.attempt != start.lease.attempt
        || start.run_key.fencing_token != start.lease.fencing_token
        || start.run_key.payload_digest != start.job.payload_digest
        || start.worker_session_id.0.is_empty()
        || serde_json::to_value(start.workspace_revision)
            .ok()
            .and_then(|value| serde_json::from_value::<WorkspaceRevision>(value).ok())
            .as_ref()
            != Some(start.workspace_revision)
    {
        return Err(ProductionCodexError::new(
            ProductionCodexErrorKind::Authority,
            "Worker dispatch authority does not match the Codex run",
        ));
    }
    Ok(())
}

fn session_identity(start: CodexThreadStart<'_>, thread_id: CodexThreadId) -> SessionIdentity {
    let (product_session_id, stage_run_id) = match &start.job.scope {
        ExecutionScope::ProductSessionExecutionScope(scope) => {
            (scope.product_session_id.clone(), None)
        }
        ExecutionScope::DeliveryStageExecutionScope(scope) => (
            scope.product_session_id.clone(),
            Some(scope.stage_run_id.clone()),
        ),
    };
    SessionIdentity {
        codex_thread_id: thread_id,
        product_session_id,
        stage_run_id,
        worker_session_id: start.worker_session_id.clone(),
    }
}

fn session_options(
    config: &ProductionCodexConfig,
    workspace: &Path,
    role_policy: Option<RoleSessionPolicy>,
) -> SessionOptions {
    SessionOptions {
        cwd: workspace.to_path_buf(),
        provider: config.provider.clone(),
        model: config.model.clone(),
        role_policy,
    }
}

const fn role_execution_mode(execution_mode: ExecutionMode) -> RoleExecutionMode {
    match execution_mode {
        ExecutionMode::React => RoleExecutionMode::React,
        ExecutionMode::DelegatedPatchShadow | ExecutionMode::DelegatedPatch => {
            RoleExecutionMode::DelegatedBatch
        }
    }
}

fn configured_role_session_policy(
    config: &ProductionCodexConfig,
    job: &ExecutionJob,
) -> Result<Option<RoleSessionPolicy>, ProductionCodexError> {
    role_session_policy(job, role_execution_mode(config.execution_mode)).map_err(|_| invalid_job())
}

fn load_runtime_messages(
    store: &mut AdapterStore,
    binding: &ModelRunBinding,
) -> Result<VecDeque<RuntimeEventMessage>, ProductionCodexError> {
    let identity = RuntimeReplayIdentity {
        lease: binding.authority.lease.clone(),
        worker_session_id: binding.authority.worker_session_id.clone(),
        session_identity: binding.authority.session_identity.clone(),
        codex_thread_id: binding.canonical_thread_id.clone(),
    };
    let snapshot = ReplayStore::load(store, &identity.stream_key())
        .map_err(map_store_error)?
        .unwrap_or_default();
    let ack_sequence = snapshot.ack_sequence;
    snapshot
        .events
        .into_iter()
        .filter(|frame| frame.sequence > ack_sequence)
        .map(|frame| serde_json::from_slice(&frame.frame).map_err(|_| unavailable()))
        .collect()
}

fn submission_input_digest(
    input: &str,
    options: &TurnSubmissionOptions,
) -> Result<Sha256Digest, ProductionCodexError> {
    let mut digest = Sha256::new();
    digest.update(b"winwincode.codex-submission.v2\0");
    digest.update((input.len() as u64).to_be_bytes());
    digest.update(input.as_bytes());
    let schema =
        serde_json::to_vec(&options.final_output_json_schema).map_err(|_| unavailable())?;
    digest.update((schema.len() as u64).to_be_bytes());
    digest.update(schema);
    Ok(Sha256Digest(format!("sha256:{:x}", digest.finalize())))
}

fn turn_submission_options(record: &StoredRun) -> TurnSubmissionOptions {
    TurnSubmissionOptions {
        final_output_json_schema: is_delegated_composer(record)
            .then(change_batch_proposal_json_schema),
    }
}

fn is_delegated_composer(record: &StoredRun) -> bool {
    record.role_policy.as_ref().is_some_and(|policy| {
        policy.execution_mode == RoleExecutionMode::DelegatedBatch
            && matches!(
                policy.role_id,
                RoleSessionPolicyRoleId::Executor | RoleSessionPolicyRoleId::Remediator
            )
    })
}

fn is_format_repair_turn(record: &StoredRun, turn_id: &str) -> bool {
    record
        .format_repair
        .as_ref()
        .is_some_and(|repair| repair.turn_id == turn_id)
}

fn is_active_format_repair(record: &StoredRun) -> bool {
    record.format_repair.as_ref().is_some_and(|repair| {
        repair.submitted
            && record.current_turn_id.as_deref() == Some(repair.turn_id.as_str())
            && record.batch_intent.is_none()
            && record.terminal.is_none()
    })
}

fn delegated_change_batch_event(
    record: &StoredRun,
    binding: &ModelRunBinding,
    turn_id: &str,
    final_message: Option<&str>,
    occurred_at: &Instant,
) -> Result<ChangeBatchProposalEvent, ProductionCodexError> {
    let final_message = final_message.ok_or_else(invalid_delegated_output)?;
    let proposal: ChangeBatchProposal =
        serde_json::from_str(final_message).map_err(|_| invalid_delegated_output())?;
    validate_delegated_proposal(&record.job, &proposal)?;
    validate_delegated_patch(&proposal.patch)?;
    let patch_digest = Sha256Digest(format!(
        "sha256:{:x}",
        Sha256::digest(proposal.patch.as_bytes())
    ));
    let batch_id = derive_change_batch_id(&binding.run_key, turn_id, None, &patch_digest)
        .map_err(|_| invalid_delegated_output())?;
    let lease = &binding.authority.lease;
    let event = ChangeBatchProposalEvent {
        identity: ChangeBatchIdentity {
            attempt: lease.attempt,
            batch_id,
            call_id: None,
            fencing_token: lease.fencing_token.clone(),
            job_id: lease.job_id.clone(),
            lease_id: lease.lease_id.clone(),
            patch_digest,
            repository_id: record.job.workspace.repository_id.clone(),
            run_key: binding.run_key.clone(),
            session_identity: binding.authority.session_identity.clone(),
            turn_id: turn_id.to_owned(),
            workspace_revision: record.workspace_revision.clone(),
        },
        occurred_at: occurred_at.clone(),
        proposal,
    };
    validate_change_batch_identity_derivation(&event.identity)
        .map_err(|_| invalid_delegated_output())?;
    let bytes = serde_json::to_vec(&event).map_err(|_| invalid_delegated_output())?;
    serde_json::from_slice(&bytes).map_err(|_| invalid_delegated_output())
}

fn validate_stored_batch_intent(
    record: &StoredRun,
    binding: &ModelRunBinding,
    intent: &StoredBatchIntent,
) -> Result<(), ProductionCodexError> {
    if !is_delegated_composer(record) {
        return Err(conflict());
    }
    validate_change_batch_identity_derivation(&intent.event.identity).map_err(|_| conflict())?;
    let proposal = serde_json::to_string(&intent.event.proposal).map_err(|_| conflict())?;
    let expected = delegated_change_batch_event(
        record,
        binding,
        &intent.event.identity.turn_id,
        Some(&proposal),
        &intent.event.occurred_at,
    )?;
    if expected != intent.event {
        return Err(conflict());
    }
    Ok(())
}

fn validate_delegated_patch(patch: &str) -> Result<(), ProductionCodexError> {
    if patch.len() > 524_288 {
        return Err(invalid_delegated_output());
    }
    let parsed = parse_patch(patch).map_err(|_| invalid_delegated_output())?;
    if parsed.hunks.is_empty() || parsed.hunks.len() > 100 {
        return Err(invalid_delegated_output());
    }
    let mut files = std::collections::HashSet::new();
    for hunk in &parsed.hunks {
        match hunk {
            Hunk::AddFile { path, .. } | Hunk::DeleteFile { path } => {
                validate_delegated_patch_path(path)?;
                files.insert(path);
            }
            Hunk::UpdateFile {
                path, move_path, ..
            } => {
                validate_delegated_patch_path(path)?;
                files.insert(path);
                if let Some(move_path) = move_path {
                    validate_delegated_patch_path(move_path)?;
                    files.insert(move_path);
                }
            }
        }
        if files.len() > 20 {
            return Err(invalid_delegated_output());
        }
    }
    Ok(())
}

fn validate_delegated_proposal(
    job: &ExecutionJob,
    proposal: &ChangeBatchProposal,
) -> Result<(), ProductionCodexError> {
    let expected = job
        .stage_input
        .as_ref()
        .and_then(|input| input.task.as_ref())
        .map(|task| {
            task.acceptance_criterion_ids
                .iter()
                .map(String::as_str)
                .collect::<std::collections::HashSet<_>>()
        })
        .ok_or_else(invalid_delegated_output)?;
    let mut observed = std::collections::HashSet::new();
    if proposal.acceptance_criteria_ids.len() != expected.len()
        || proposal
            .acceptance_criteria_ids
            .iter()
            .any(|criterion| !expected.contains(criterion.as_str()) || !observed.insert(criterion))
    {
        return Err(invalid_delegated_output());
    }
    Ok(())
}

fn validate_delegated_patch_path(path: &Path) -> Result<(), ProductionCodexError> {
    if path.as_os_str().is_empty()
        || path.is_absolute()
        || path
            .components()
            .any(|component| !matches!(component, Component::Normal(_)))
    {
        return Err(invalid_delegated_output());
    }
    Ok(())
}

fn invalid_delegated_output() -> ProductionCodexError {
    ProductionCodexError::new(
        ProductionCodexErrorKind::Conflict,
        "delegated ChangeBatch proposal is invalid",
    )
}

fn canonical_workspace(workspace: &Path) -> Result<PathBuf, ProductionCodexError> {
    if !workspace.is_absolute() || !workspace.is_dir() {
        return Err(invalid_configuration());
    }
    workspace
        .canonicalize()
        .map_err(|_| invalid_configuration())
}

fn canonical_id(prefix: &str, namespace: &[u8], run_key: &str, sequence: u64) -> String {
    let mut digest = Sha256::new();
    digest.update(namespace);
    digest.update([0]);
    digest.update(run_key.as_bytes());
    digest.update([0]);
    digest.update(sequence.to_be_bytes());
    let hex = format!("{:x}", digest.finalize());
    format!("{prefix}_{}", &hex[..26].to_ascii_uppercase())
}

fn canonical_parts_id(prefix: &str, namespace: &[u8], parts: &[&[u8]]) -> String {
    let mut digest = Sha256::new();
    digest.update(namespace);
    for part in parts {
        digest.update((part.len() as u64).to_be_bytes());
        digest.update(part);
    }
    let hex = format!("{:x}", digest.finalize());
    format!("{prefix}_{}", &hex[..26].to_ascii_uppercase())
}

fn private_payload_digest(
    namespace: &[u8],
    value: &impl Serialize,
) -> Result<String, ProductionCodexError> {
    let encoded = serde_json::to_vec(value).map_err(|_| unavailable())?;
    let mut digest = Sha256::new();
    digest.update(namespace);
    digest.update([0]);
    digest.update(encoded);
    Ok(format!("sha256:{:x}", digest.finalize()))
}

fn non_empty(value: String) -> Option<String> {
    (!value.trim().is_empty()).then_some(value)
}

fn verification_evidence_status(
    status: &codex_protocol::protocol::ExecCommandStatus,
) -> crate::stage_product::VerificationEvidenceStatus {
    match status {
        codex_protocol::protocol::ExecCommandStatus::Completed => {
            crate::stage_product::VerificationEvidenceStatus::Completed
        }
        codex_protocol::protocol::ExecCommandStatus::Failed => {
            crate::stage_product::VerificationEvidenceStatus::Failed
        }
        codex_protocol::protocol::ExecCommandStatus::Declined => {
            crate::stage_product::VerificationEvidenceStatus::Declined
        }
    }
}

fn validation_command(command: &[String]) -> bool {
    let normalized = command
        .iter()
        .map(|part| part.to_ascii_lowercase())
        .collect::<Vec<_>>()
        .join(" ");
    [
        "cargo test",
        "cargo nextest",
        "cargo check",
        "cargo clippy",
        "pnpm test",
        "pnpm typecheck",
        "pnpm lint",
        "pnpm build",
        "pnpm verify",
        "npm test",
        "npm run test",
        "npm run typecheck",
        "npm run lint",
        "npm run build",
        "yarn test",
        "bun test",
        "pytest",
        "python -m pytest",
        "go test",
        "dotnet test",
        "swift test",
        "gradle test",
        "mvn test",
    ]
    .iter()
    .any(|needle| normalized.contains(needle))
}

fn valid_prefixed_id(value: &str, prefix: &str) -> bool {
    value.len() == prefix.len() + 26
        && value.starts_with(prefix)
        && value[prefix.len()..]
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric())
}

fn valid_route_token(value: &str) -> bool {
    let value = value.trim();
    !value.is_empty()
        && value.len() <= 200
        && value.bytes().all(|byte| {
            byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.' | b'/' | b':')
        })
}

fn valid_sha256_digest(value: &str) -> bool {
    value.len() == 71
        && value.starts_with("sha256:")
        && value[7..].bytes().all(|byte| byte.is_ascii_hexdigit())
}

fn canonical_instant(instant: &Instant) -> bool {
    let value = instant.0.as_bytes();
    value.len() == 24
        && value[4] == b'-'
        && value[7] == b'-'
        && value[10] == b'T'
        && value[13] == b':'
        && value[16] == b':'
        && value[19] == b'.'
        && value[23] == b'Z'
        && value.iter().enumerate().all(|(index, byte)| {
            matches!(index, 4 | 7 | 10 | 13 | 16 | 19 | 23) || byte.is_ascii_digit()
        })
        && number(value, 5, 7).is_some_and(|month| (1..=12).contains(&month))
        && number(value, 8, 10).is_some_and(|day| (1..=31).contains(&day))
        && number(value, 11, 13).is_some_and(|hour| hour <= 23)
        && number(value, 14, 16).is_some_and(|minute| minute <= 59)
        && number(value, 17, 19).is_some_and(|second| second <= 59)
}

fn number(value: &[u8], start: usize, end: usize) -> Option<u8> {
    value
        .get(start..end)?
        .iter()
        .try_fold(0_u8, |number, byte| {
            number.checked_mul(10)?.checked_add(byte - b'0')
        })
}

const SEALED_HELPER_DIRECTORY: &str = "helper-installation";
const SEALED_HELPER_NAME: &str = "winwincode-kernel-helper";

#[cfg(unix)]
static HELPER_VALIDATIONS: Mutex<Vec<String>> = Mutex::new(Vec::new());

fn project_helper(path: &Path, manifest: &HelperReleaseManifest) -> Option<Arc<[u8]>> {
    let Ok(metadata) = std::fs::symlink_metadata(path) else {
        return None;
    };
    if metadata.file_type().is_symlink() || !metadata.is_file() {
        return None;
    }
    let Ok(canonical_path) = path.canonicalize() else {
        return None;
    };
    let Ok(current_executable) = std::env::current_exe().and_then(|path| path.canonicalize())
    else {
        return None;
    };
    let current_directory = current_executable.parent()?;
    let release_directory =
        if current_directory.file_name().and_then(|name| name.to_str()) == Some("deps") {
            current_directory.parent().unwrap_or(current_directory)
        } else {
            current_directory
        };
    if manifest.path().parent() != Some(release_directory)
        || canonical_path.parent() != Some(release_directory)
        || !canonical_path.is_file()
        || canonical_path.file_name().and_then(|name| name.to_str()) != Some(manifest.binary_path())
    {
        return None;
    }
    #[cfg(unix)]
    {
        let expected = format!(
            "{{\"protocol\":\"winwincode-kernel-helper\",\"version\":1,\"packageVersion\":\"{}\"}}\n",
            env!("CARGO_PKG_VERSION")
        );
        let identity = format!(
            "{{\"protocol\":\"winwincode-kernel-helper\",\"version\":1,\"packageVersion\":\"{}\",\"sourceSha256\":\"{}\"}}\n",
            env!("CARGO_PKG_VERSION"),
            env!("WINWINCODE_HELPER_SOURCE_SHA256")
        );
        validate_helper_image(
            &canonical_path,
            manifest,
            expected.as_bytes(),
            identity.as_bytes(),
        )
    }
    #[cfg(not(unix))]
    {
        None
    }
}

/// Opens and copies the already-validated helper through one file descriptor,
/// then runs all probes against the sealed bytes.  A later replacement of the
/// caller path cannot affect Kernel self-exec because the Kernel receives only
/// this private installation.
fn seal_helper(
    source: &Path,
    validated_source: Option<&[u8]>,
    data_directory: &Path,
    manifest: &HelperReleaseManifest,
) -> Result<PathBuf, ProductionCodexError> {
    let destination_directory = data_directory.join(SEALED_HELPER_DIRECTORY);
    ensure_private_directory(&destination_directory).map_err(|_| unavailable())?;
    let destination = destination_directory.join(SEALED_HELPER_NAME);
    if std::fs::symlink_metadata(&destination).is_ok() {
        let valid = validate_sealed_helper(&destination, manifest)
            || (repair_sealed_helper_permissions(&destination, manifest)
                && validate_sealed_helper(&destination, manifest));
        return valid
            .then_some(destination)
            .ok_or_else(invalid_configuration);
    }

    let source_bytes;
    let bytes = if let Some(bytes) = validated_source {
        bytes
    } else {
        source_bytes = read_helper_bytes(source).map_err(|_| invalid_configuration())?;
        &source_bytes
    };
    if source.file_name().and_then(|name| name.to_str()) != Some(manifest.binary_path()) {
        return Err(invalid_configuration());
    }
    #[cfg(unix)]
    if validated_source.is_none()
        && source.metadata().map_or(true, |metadata| {
            metadata.permissions().mode() & 0o777 != manifest.binary_mode()
        })
    {
        return Err(invalid_configuration());
    }
    if helper_digest(bytes) != manifest.binary_digest().0 {
        return Err(invalid_configuration());
    }
    let temporary = destination_directory.join(format!(".{SEALED_HELPER_NAME}.{}", Uuid::now_v7()));
    let mut file = OpenOptions::new();
    file.create_new(true).write(true);
    #[cfg(unix)]
    file.mode(0o700);
    let mut file = file.open(&temporary).map_err(|_| unavailable())?;
    file.write_all(bytes).map_err(|_| unavailable())?;
    file.sync_all().map_err(|_| unavailable())?;
    drop(file);
    #[cfg(unix)]
    std::fs::set_permissions(&temporary, std::fs::Permissions::from_mode(0o700))
        .map_err(|_| unavailable())?;
    if let Err(error) = std::fs::rename(&temporary, &destination) {
        let _ = std::fs::remove_file(&temporary);
        if error.kind() != std::io::ErrorKind::AlreadyExists {
            return Err(unavailable());
        }
    }
    sync_directory(&destination_directory).map_err(|_| unavailable())?;
    validate_sealed_helper(&destination, manifest)
        .then_some(destination)
        .ok_or_else(invalid_configuration)
}

/// Crash fixtures and some archive restore tools preserve private bytes but
/// lose the executable mode bit. Repair only that metadata after the sealed
/// regular-file bytes and digest have already matched; a symlink or changed
/// helper is still rejected.
#[cfg(unix)]
fn repair_sealed_helper_permissions(path: &Path, manifest: &HelperReleaseManifest) -> bool {
    let Ok(metadata) = std::fs::symlink_metadata(path) else {
        return false;
    };
    if metadata.file_type().is_symlink()
        || !metadata.is_file()
        || read_helper_bytes(path)
            .ok()
            .is_none_or(|bytes| helper_digest(&bytes) != manifest.binary_digest().0)
    {
        return false;
    }
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o700)).is_ok()
}

#[cfg(not(unix))]
fn repair_sealed_helper_permissions(_path: &Path, _manifest: &HelperReleaseManifest) -> bool {
    false
}

fn validate_sealed_helper(path: &Path, manifest: &HelperReleaseManifest) -> bool {
    let Ok(metadata) = std::fs::symlink_metadata(path) else {
        return false;
    };
    if metadata.file_type().is_symlink() || !metadata.is_file() {
        return false;
    }
    #[cfg(unix)]
    if metadata.permissions().mode() & 0o777 != 0o700 {
        return false;
    }
    let Ok(bytes) = read_helper_bytes(path) else {
        return false;
    };
    if helper_digest(&bytes) != manifest.binary_digest().0 {
        return false;
    }
    #[cfg(unix)]
    {
        let expected = format!(
            "{{\"protocol\":\"winwincode-kernel-helper\",\"version\":1,\"packageVersion\":\"{}\"}}\n",
            env!("CARGO_PKG_VERSION")
        );
        let identity = format!(
            "{{\"protocol\":\"winwincode-kernel-helper\",\"version\":1,\"packageVersion\":\"{}\",\"sourceSha256\":\"{}\"}}\n",
            env!("CARGO_PKG_VERSION"),
            env!("WINWINCODE_HELPER_SOURCE_SHA256")
        );
        bounded_helper_handshake(path, expected.as_bytes())
            && bounded_helper_identity(path, identity.as_bytes())
    }
    #[cfg(not(unix))]
    {
        false
    }
}

fn read_helper_bytes(path: &Path) -> std::io::Result<Vec<u8>> {
    let metadata = std::fs::symlink_metadata(path)?;
    if metadata.file_type().is_symlink() || !metadata.is_file() || metadata.len() > MAX_HELPER_BYTES
    {
        return Err(std::io::Error::other("helper is not a regular file"));
    }
    let mut options = OpenOptions::new();
    options.read(true);
    #[cfg(unix)]
    options.custom_flags(if cfg!(target_os = "macos") {
        // Darwin O_NOFOLLOW.
        0x100
    } else {
        // Linux O_NOFOLLOW.  Release targets are Darwin and Linux.
        0x20_000
    });
    let mut file = options.open(path)?;
    let opened_metadata = file.metadata()?;
    if !opened_metadata.is_file() {
        return Err(std::io::Error::other("helper is not a regular file"));
    }
    let mut bytes = Vec::new();
    file.read_to_end(&mut bytes)?;
    if bytes.len() as u64 > MAX_HELPER_BYTES {
        return Err(std::io::Error::other("helper is too large"));
    }
    Ok(bytes)
}

fn helper_digest(bytes: &[u8]) -> String {
    format!("sha256:{:x}", Sha256::digest(bytes))
}

fn ensure_private_directory(path: &Path) -> std::io::Result<()> {
    match std::fs::symlink_metadata(path) {
        Ok(metadata) => {
            if metadata.file_type().is_symlink() || !metadata.is_dir() {
                return Err(std::io::Error::other(
                    "helper installation directory is not private",
                ));
            }
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            std::fs::create_dir_all(path)?;
        }
        Err(error) => return Err(error),
    }
    let metadata = std::fs::symlink_metadata(path)?;
    if metadata.file_type().is_symlink() || !metadata.is_dir() {
        return Err(std::io::Error::other(
            "helper installation directory is not private",
        ));
    }
    restrict_directory(path)
}

#[cfg(unix)]
fn restrict_directory(path: &Path) -> std::io::Result<()> {
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o700))
}

#[cfg(not(unix))]
fn restrict_directory(_path: &Path) -> std::io::Result<()> {
    Ok(())
}

#[cfg(unix)]
fn sync_directory(path: &Path) -> std::io::Result<()> {
    std::fs::File::open(path)?.sync_all()
}

#[cfg(not(unix))]
fn sync_directory(_path: &Path) -> std::io::Result<()> {
    Ok(())
}

#[cfg(unix)]
fn bounded_helper_handshake(path: &Path, expected: &[u8]) -> bool {
    bounded_helper_probe(path, "--winwincode-helper-handshake", expected)
}

#[cfg(unix)]
fn bounded_helper_identity(path: &Path, expected: &[u8]) -> bool {
    bounded_helper_probe(path, "--winwincode-helper-identity", expected)
}

#[cfg(unix)]
fn validate_helper_image(
    path: &Path,
    manifest: &HelperReleaseManifest,
    handshake: &[u8],
    identity: &[u8],
) -> Option<Arc<[u8]>> {
    let Ok(mut validated) = HELPER_VALIDATIONS.lock() else {
        return None;
    };
    let bytes = read_helper_bytes(path).ok()?;
    if helper_digest(&bytes) != manifest.binary_digest().0
        || !path.metadata().is_ok_and(|metadata| {
            metadata.permissions().mode() & 0o777 == manifest.binary_mode()
                && manifest.binary_mode() == HELPER_RELEASE_BINARY_MODE
        })
    {
        return None;
    }
    let validation_key = format!(
        "{}\0{}\0{}",
        manifest.binary_digest().0,
        manifest.package_version(),
        manifest.source_sha256()
    );
    if validated.contains(&validation_key) {
        return Some(bytes.into());
    }
    let probes_succeeded =
        || bounded_helper_handshake(path, handshake) && bounded_helper_identity(path, identity);
    if !probes_succeeded() {
        std::thread::yield_now();
        if !probes_succeeded() {
            return None;
        }
    }
    if validated.len() >= 32 {
        validated.remove(0);
    }
    validated.push(validation_key);
    Some(bytes.into())
}

#[cfg(unix)]
fn bounded_helper_probe(path: &Path, argument: &str, expected: &[u8]) -> bool {
    use std::os::fd::OwnedFd;
    use std::os::unix::net::UnixStream;
    use std::os::unix::process::CommandExt as _;

    const OUTPUT_LIMIT: u64 = 4096;
    const TIMEOUT: Duration = Duration::from_secs(2);
    const CLEANUP_TIMEOUT: Duration = Duration::from_secs(1);

    let Ok((stdout, helper_stdout)) = UnixStream::pair() else {
        return false;
    };
    let Ok((stderr, helper_stderr)) = UnixStream::pair() else {
        return false;
    };
    let output_deadline = std::time::Instant::now() + TIMEOUT + CLEANUP_TIMEOUT;

    let mut command = std::process::Command::new(path);
    command
        .arg(argument)
        .stdin(Stdio::null())
        .stdout(Stdio::from(OwnedFd::from(helper_stdout)))
        .stderr(Stdio::from(OwnedFd::from(helper_stderr)))
        .process_group(0);
    let Ok(mut child) = command.spawn() else {
        return false;
    };
    drop(command);
    let stdout = std::thread::spawn(move || {
        read_bounded_helper_output(stdout, OUTPUT_LIMIT, output_deadline)
    });
    let stderr = std::thread::spawn(move || {
        read_bounded_helper_output(stderr, OUTPUT_LIMIT, output_deadline)
    });
    let started = std::time::Instant::now();
    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break Some(status),
            Ok(None) if started.elapsed() < TIMEOUT => {
                std::thread::sleep(Duration::from_millis(10));
            }
            Ok(None) | Err(_) => {
                break None;
            }
        }
    };
    if !terminate_helper_process_group(&mut child, CLEANUP_TIMEOUT) {
        return false;
    }
    let Ok(Ok(stdout)) = stdout.join() else {
        return false;
    };
    let Ok(Ok(stderr)) = stderr.join() else {
        return false;
    };
    status.is_some_and(|status| status.success()) && stderr.is_empty() && stdout == expected
}

#[cfg(unix)]
fn read_bounded_helper_output(
    mut output: std::os::unix::net::UnixStream,
    limit: u64,
    deadline: std::time::Instant,
) -> std::io::Result<Vec<u8>> {
    const READ_TIMEOUT: Duration = Duration::from_millis(25);

    output.set_read_timeout(Some(READ_TIMEOUT))?;
    let mut bytes = Vec::new();
    let mut chunk = [0_u8; 1024];
    while std::time::Instant::now() < deadline && bytes.len() as u64 <= limit {
        let remaining = usize::try_from(limit + 1 - bytes.len() as u64)
            .unwrap_or(chunk.len())
            .min(chunk.len());
        match output.read(&mut chunk[..remaining]) {
            Ok(0) => return Ok(bytes),
            Ok(count) => bytes.extend_from_slice(&chunk[..count]),
            Err(error)
                if matches!(
                    error.kind(),
                    std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut
                ) => {}
            Err(error) => return Err(error),
        }
    }
    if bytes.len() as u64 > limit {
        Ok(bytes)
    } else {
        Err(std::io::Error::new(
            std::io::ErrorKind::TimedOut,
            "helper output did not close before the deadline",
        ))
    }
}

#[cfg(unix)]
fn terminate_helper_process_group(child: &mut std::process::Child, timeout: Duration) -> bool {
    let _ = std::process::Command::new("/bin/kill")
        .args(["-KILL", "--", &format!("-{}", child.id())])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status();
    let _ = child.kill();
    let deadline = std::time::Instant::now() + timeout;
    loop {
        match child.try_wait() {
            Ok(Some(_)) => return true,
            Ok(None) if std::time::Instant::now() < deadline => {
                std::thread::sleep(Duration::from_millis(10));
            }
            Ok(None) | Err(_) => return false,
        }
    }
}

fn decode_kernel_event(payload_json: &str) -> Result<CodexEvent, ProductionCodexError> {
    serde_json::from_str(payload_json).map_err(|_| kernel_error())
}

/// Stable adapter failure category.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ProductionCodexErrorKind {
    InvalidConfiguration,
    Authority,
    Conflict,
    DurableState,
    ModelBridge,
    Kernel,
    Restart,
    UnknownThread,
}

/// Secret-safe adapter failure.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ProductionCodexError {
    kind: ProductionCodexErrorKind,
    message: &'static str,
}

impl ProductionCodexError {
    const fn new(kind: ProductionCodexErrorKind, message: &'static str) -> Self {
        Self { kind, message }
    }

    #[must_use]
    pub const fn kind(&self) -> ProductionCodexErrorKind {
        self.kind
    }
}

impl fmt::Display for ProductionCodexError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.message)
    }
}

impl std::error::Error for ProductionCodexError {}

fn invalid_configuration() -> ProductionCodexError {
    ProductionCodexError::new(
        ProductionCodexErrorKind::InvalidConfiguration,
        "production Codex configuration is invalid",
    )
}

fn invalid_job() -> ProductionCodexError {
    ProductionCodexError::new(
        ProductionCodexErrorKind::Authority,
        "ExecutionJob is not valid for the embedded Codex session",
    )
}

fn unavailable() -> ProductionCodexError {
    ProductionCodexError::new(
        ProductionCodexErrorKind::DurableState,
        "production Codex durable state is unavailable",
    )
}

fn conflict() -> ProductionCodexError {
    ProductionCodexError::new(
        ProductionCodexErrorKind::Conflict,
        "production Codex run conflicts with durable state",
    )
}

fn kernel_error() -> ProductionCodexError {
    ProductionCodexError::new(
        ProductionCodexErrorKind::Kernel,
        "embedded Codex Kernel operation failed",
    )
}

fn unknown_thread() -> ProductionCodexError {
    ProductionCodexError::new(
        ProductionCodexErrorKind::UnknownThread,
        "embedded Codex thread is not registered",
    )
}

fn map_store_error(error: AdapterStoreError) -> ProductionCodexError {
    match error {
        AdapterStoreError::Conflict => conflict(),
        AdapterStoreError::Unavailable | AdapterStoreError::Corrupt => unavailable(),
    }
}

fn map_bridge_error(_: BridgeError) -> ProductionCodexError {
    ProductionCodexError::new(
        ProductionCodexErrorKind::ModelBridge,
        "embedded Codex model bridge operation failed",
    )
}

#[cfg(test)]
mod tests {
    use super::{
        AdapterStore, MAX_HELPER_BYTES, ModelLeaseAuthority, ModelRunBinding,
        ProductionCodexErrorKind, RoleExecutionMode, StoredRun, StoredRunPhase,
        TurnSubmissionOptions, bounded_helper_handshake, decode_kernel_event,
        delegated_change_batch_event, load_stored_run, migrate_stored_run_role_policies_v1_to_v2,
        project_helper, read_helper_bytes, role_session_policy, seal_helper,
        submission_input_digest, terminate_helper_process_group, turn_submission_options,
        validate_delegated_patch, validate_delegated_patch_path, validate_helper_image,
        validate_sealed_helper, validate_stored_batch_intent,
    };
    use crate::helper_release::HelperReleaseManifest;
    use std::fmt::Write as _;
    use std::path::PathBuf;
    use winwincode_domain::{
        ChangeBatchId, CodexThreadId, DeliveryId, DeliveryTaskId, ExecutionJobId, FencingToken,
        Instant, LeaseId, ProductSessionId, RepositoryId, SchemaVersion, SessionIdentity,
        Sha256Digest, StageRunId, WorkerId, WorkerInstanceId, WorkerSessionId, WorkspaceRevision,
    };
    use winwincode_execution_port::generated::{
        DeliveryStageAcceptanceCriterionInput, DeliveryStageExecutionScope,
        DeliveryStageExecutionScopeKind, DeliveryStageInput, DeliveryStageTaskInput, ExecutionJob,
        ExecutionLeaseStamp, ExecutionLimits, ExecutionScope, ExecutionWorkspace,
        ExecutionWorkspaceWriteMode,
    };

    fn executor_job() -> ExecutionJob {
        let task_id = DeliveryTaskId("dtk_00000000000000000000000001".to_owned());
        ExecutionJob {
            attempt: 1,
            execution_profile: "executor".to_owned(),
            goal: "Implement fixture".to_owned(),
            job_id: ExecutionJobId("job_00000000000000000000000001".to_owned()),
            limits: ExecutionLimits {
                deadline_at: Instant("2026-08-28T00:00:00Z".to_owned()),
                max_artifact_bytes: 1_048_576,
                max_runtime_seconds: 300,
            },
            payload_digest: Sha256Digest(format!("sha256:{}", "a".repeat(64))),
            scope: ExecutionScope::DeliveryStageExecutionScope(DeliveryStageExecutionScope {
                delivery_id: DeliveryId("dlv_00000000000000000000000001".to_owned()),
                delivery_task_id: Some(task_id.clone()),
                kind: DeliveryStageExecutionScopeKind::DeliveryStage,
                product_session_id: ProductSessionId("ses_00000000000000000000000001".to_owned()),
                rework_authorization: None,
                stage_run_id: StageRunId("run_00000000000000000000000001".to_owned()),
            }),
            stage_input: Some(DeliveryStageInput {
                acceptance_criteria: vec![DeliveryStageAcceptanceCriterionInput {
                    criterion_id: "criterion-fixture".to_owned(),
                    description: "The exact fixture behavior is verified.".to_owned(),
                    required: true,
                    verification_method: Some("Run the exact fixture check.".to_owned()),
                }],
                candidate_ref: None,
                constraints: vec!["Keep the exact repository boundary.".to_owned()],
                delivery_spec_id: "spec-fixture".to_owned(),
                delivery_spec_revision: 2,
                goal: "Implement fixture".to_owned(),
                out_of_scope: Vec::new(),
                schema_version: SchemaVersion::WinwincodeV1,
                scope: vec!["Fixture source".to_owned()],
                task: Some(DeliveryStageTaskInput {
                    acceptance_criterion_ids: vec!["criterion-fixture".to_owned()],
                    goal: "Implement fixture".to_owned(),
                    task_id,
                    title: "Implement fixture".to_owned(),
                }),
                title: "Fixture Delivery".to_owned(),
            }),
            workspace: ExecutionWorkspace {
                checkout_revision: "main".to_owned(),
                repository_id: RepositoryId("repo_00000000000000000000000001".to_owned()),
                write_mode: ExecutionWorkspaceWriteMode::Candidate,
            },
        }
    }

    #[cfg(unix)]
    fn helper_fixture(root: &std::path::Path) -> PathBuf {
        use std::os::unix::fs::PermissionsExt as _;

        let helper = root.join("winwincode-kernel-helper");
        std::fs::write(
            &helper,
            format!(
                "#!/bin/sh\ncase \"$1\" in\n  --winwincode-helper-handshake) printf '%s\\n' '{{\"protocol\":\"winwincode-kernel-helper\",\"version\":1,\"packageVersion\":\"{}\"}}' ;;\n  --winwincode-helper-identity) printf '%s\\n' '{{\"protocol\":\"winwincode-kernel-helper\",\"version\":1,\"packageVersion\":\"{}\",\"sourceSha256\":\"{}\"}}' ;;\n  *) exit 2 ;;\nesac\n",
                env!("CARGO_PKG_VERSION"),
                env!("CARGO_PKG_VERSION"),
                env!("WINWINCODE_HELPER_SOURCE_SHA256"),
            ),
        )
        .expect("write helper fixture");
        std::fs::set_permissions(&helper, std::fs::Permissions::from_mode(0o755))
            .expect("make helper fixture executable");
        helper
    }

    #[cfg(unix)]
    #[test]
    fn oversized_helper_is_rejected_before_reading_or_sealing() {
        use std::fs::OpenOptions;
        use std::os::unix::fs::PermissionsExt as _;

        let root = test_root("oversized");
        std::fs::create_dir_all(&root).expect("create oversized helper fixture");
        let helper = root.join("winwincode-kernel-helper");
        let file = OpenOptions::new()
            .create_new(true)
            .write(true)
            .open(&helper)
            .expect("create oversized helper");
        file.set_len(MAX_HELPER_BYTES + 1)
            .expect("sparsely extend oversized helper");
        std::fs::set_permissions(&helper, std::fs::Permissions::from_mode(0o755))
            .expect("make oversized helper executable");
        assert!(read_helper_bytes(&helper).is_err());
        std::fs::remove_dir_all(root).expect("remove oversized helper fixture");
    }

    #[cfg(unix)]
    fn test_root(name: &str) -> PathBuf {
        let unique = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("system clock")
            .as_nanos();
        std::env::temp_dir().join(format!(
            "winwincode-codex-helper-{name}-{}-{unique}",
            std::process::id()
        ))
    }

    #[cfg(unix)]
    #[test]
    fn startup_migrates_v1_role_policy_once_and_runtime_loads_only_v2() {
        let root = test_root("role-policy-v1-migration");
        let store = AdapterStore::open(&root).expect("open migration store");
        let job = executor_job();
        let policy = role_session_policy(&job, RoleExecutionMode::React)
            .expect("build React policy")
            .expect("Delivery policy");
        let record = StoredRun {
            job,
            workspace_revision: WorkspaceRevision(format!("git-tree:{}", "1".repeat(40))),
            canonical_thread_id: CodexThreadId("cdx_00000000000000000000000001".to_owned()),
            job_digest: Sha256Digest(format!("sha256:{}", "b".repeat(64))),
            workspace: root.join("candidate"),
            role_policy: Some(policy),
            kernel_session_id: "kernel-session-fixture".to_owned(),
            rollout_path: None,
            submission_id: "submission-fixture".to_owned(),
            submission_digest: None,
            phase: StoredRunPhase::Prepared,
            last_tokens: 0,
            last_runtime_millis: 0,
            last_activity_at: Instant("2026-08-28T00:00:00Z".to_owned()),
            terminal: None,
            terminal_trace: None,
            current_turn_id: None,
            last_agent_message: None,
            stage_product_sources: Vec::new(),
            batch_intent: None,
            format_repair: None,
            terminal_message_id: None,
        };
        let mut legacy = serde_json::to_value(record).expect("encode stored run");
        let role_policy = legacy
            .get_mut("rolePolicy")
            .and_then(serde_json::Value::as_object_mut)
            .expect("role policy object");
        role_policy.insert("schemaVersion".to_owned(), serde_json::json!(1));
        role_policy.remove("executionMode");
        store
            .save_run("run-fixture", &legacy)
            .expect("save legacy record");

        assert!(load_stored_run(&store, "run-fixture").is_err());
        migrate_stored_run_role_policies_v1_to_v2(&store).expect("run startup migration");
        let migrated = load_stored_run(&store, "run-fixture")
            .expect("load canonical stored run")
            .expect("stored run");
        let migrated_policy = migrated.role_policy.expect("migrated role policy");
        assert_eq!(migrated_policy.schema_version, 2);
        assert_eq!(migrated_policy.execution_mode, RoleExecutionMode::React);
        let canonical: serde_json::Value = store
            .load_run("run-fixture")
            .expect("load migrated JSON")
            .expect("migrated JSON");
        assert_eq!(canonical["rolePolicy"]["schemaVersion"], 2);
        assert_eq!(canonical["rolePolicy"]["executionMode"], "react");
        drop(store);

        let restarted = AdapterStore::open(&root).expect("reopen migration store");
        migrate_stored_run_role_policies_v1_to_v2(&restarted)
            .expect("replay completed startup migration");
        let replayed = load_stored_run(&restarted, "run-fixture")
            .expect("replay migrated stored run")
            .expect("replayed stored run");
        assert_eq!(replayed.role_policy, Some(migrated_policy));
        let after_replay: serde_json::Value = restarted
            .load_run("run-fixture")
            .expect("reload canonical JSON")
            .expect("canonical JSON");
        assert_eq!(after_replay, canonical);

        restarted
            .save_run("run-fixture", &legacy)
            .expect("inject obsolete runtime shape");
        assert!(load_stored_run(&restarted, "run-fixture").is_err());
        drop(restarted);
        std::fs::remove_dir_all(root).expect("remove migration fixture");
    }

    #[cfg(unix)]
    fn process_is_running(process_id: u32) -> bool {
        std::process::Command::new("/bin/kill")
            .args(["-0", "--", &process_id.to_string()])
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status()
            .is_ok_and(|status| status.success())
    }

    #[cfg(unix)]
    fn assert_process_stops(process_id: u32) {
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(1);
        while process_is_running(process_id) && std::time::Instant::now() < deadline {
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        assert!(!process_is_running(process_id));
    }

    #[test]
    fn malformed_kernel_event_returns_only_secret_safe_error() {
        let secret = "TOKEN=TOKEN_VALUE PAYLOAD=PAYLOAD_VALUE";
        let error = decode_kernel_event(&format!("{{\"malformed\":\"{secret}\"}}"))
            .expect_err("malformed event must fail closed");
        assert_eq!(error.kind(), ProductionCodexErrorKind::Kernel);
        for rendered in [error.to_string(), format!("{error:?}")] {
            assert!(!rendered.contains("TOKEN_VALUE"));
            assert!(!rendered.contains("PAYLOAD_VALUE"));
        }
    }

    #[test]
    fn submission_digest_seals_the_exact_input_bytes() {
        let react = TurnSubmissionOptions::default();
        let delegated = TurnSubmissionOptions {
            final_output_json_schema: Some(
                crate::stage_product::change_batch_proposal_json_schema(),
            ),
        };
        let original = submission_input_digest("exact prompt", &react).expect("digest input");
        assert_eq!(
            original,
            submission_input_digest("exact prompt", &react).expect("digest same input")
        );
        assert_ne!(
            original,
            submission_input_digest("exact prompt\n", &react).expect("digest newline input")
        );
        assert_ne!(
            original,
            submission_input_digest("Exact prompt", &react).expect("digest changed input")
        );
        assert_ne!(
            original,
            submission_input_digest("exact prompt", &delegated).expect("digest schema input")
        );
        let mut changed_schema = delegated.clone();
        changed_schema.final_output_json_schema = Some(serde_json::json!({"type": "string"}));
        assert_ne!(
            submission_input_digest("exact prompt", &delegated).expect("digest canonical schema"),
            submission_input_digest("exact prompt", &changed_schema)
                .expect("digest changed schema")
        );
    }

    fn delegated_record_and_binding() -> (StoredRun, ModelRunBinding) {
        let mut job = executor_job();
        job.workspace.write_mode = ExecutionWorkspaceWriteMode::ReadOnly;
        let thread_id = CodexThreadId("cdx_00000000000000000000000001".to_owned());
        let worker_session_id = WorkerSessionId("wss_00000000000000000000000001".to_owned());
        let record = StoredRun {
            role_policy: role_session_policy(&job, RoleExecutionMode::DelegatedBatch)
                .expect("build delegated policy"),
            job,
            workspace_revision: WorkspaceRevision(format!("git-tree:{}", "1".repeat(40))),
            canonical_thread_id: thread_id.clone(),
            job_digest: Sha256Digest(format!("sha256:{}", "a".repeat(64))),
            workspace: PathBuf::from("/tmp/delegated-candidate"),
            kernel_session_id: "kernel-session-fixture".to_owned(),
            rollout_path: None,
            submission_id: "submission-fixture".to_owned(),
            submission_digest: None,
            phase: StoredRunPhase::RuntimeStarted,
            last_tokens: 0,
            last_runtime_millis: 0,
            last_activity_at: Instant("2026-08-28T00:00:00Z".to_owned()),
            terminal: None,
            terminal_trace: None,
            current_turn_id: Some("turn-fixture".to_owned()),
            last_agent_message: None,
            stage_product_sources: Vec::new(),
            batch_intent: None,
            format_repair: None,
            terminal_message_id: None,
        };
        let binding = ModelRunBinding {
            run_key: format!("sha256:{}", "b".repeat(64)),
            canonical_thread_id: thread_id.clone(),
            kernel_session_id: "kernel-session-fixture".to_owned(),
            authority: ModelLeaseAuthority {
                lease: ExecutionLeaseStamp {
                    attempt: 1,
                    expires_at: Instant("2026-08-28T01:00:00Z".to_owned()),
                    fencing_token: FencingToken("fence-fixture".to_owned()),
                    issued_at: Instant("2026-08-28T00:00:00Z".to_owned()),
                    job_id: record.job.job_id.clone(),
                    lease_id: LeaseId("lease-fixture".to_owned()),
                    worker_id: WorkerId("wrk_00000000000000000000000001".to_owned()),
                    worker_instance_id: WorkerInstanceId(
                        "wki_00000000000000000000000001".to_owned(),
                    ),
                },
                worker_session_id: worker_session_id.clone(),
                session_identity: SessionIdentity {
                    codex_thread_id: thread_id,
                    product_session_id: ProductSessionId(
                        "ses_00000000000000000000000001".to_owned(),
                    ),
                    stage_run_id: Some(StageRunId("run_00000000000000000000000001".to_owned())),
                    worker_session_id,
                },
            },
            opened_at: Instant("2026-08-28T00:00:00Z".to_owned()),
        };
        (record, binding)
    }

    #[test]
    fn delegated_final_output_is_strict_and_has_a_deterministic_batch_identity() {
        let (mut record, binding) = delegated_record_and_binding();
        assert!(
            turn_submission_options(&record)
                .final_output_json_schema
                .is_some()
        );
        let mut react = record.clone();
        react
            .role_policy
            .as_mut()
            .expect("executor policy")
            .execution_mode = RoleExecutionMode::React;
        assert!(
            turn_submission_options(&react)
                .final_output_json_schema
                .is_none()
        );
        let output = serde_json::json!({
            "acceptanceCriteriaIds": ["criterion-fixture"],
            "disposition": "final",
            "patch": "*** Begin Patch\n*** Update File: src/lib.rs\n@@\n-old\n+new\n*** End Patch\n",
            "schemaVersion": 1,
            "validationProfile": "changed"
        });
        let occurred_at = Instant("2026-08-28T00:05:00Z".to_owned());
        let first = delegated_change_batch_event(
            &record,
            &binding,
            "turn-fixture",
            Some(&output.to_string()),
            &occurred_at,
        )
        .expect("accept canonical proposal");
        let later = delegated_change_batch_event(
            &record,
            &binding,
            "turn-fixture",
            Some(&output.to_string()),
            &Instant("2026-08-28T00:06:00Z".to_owned()),
        )
        .expect("rebuild canonical proposal");
        assert_eq!(first.identity, later.identity);
        assert_eq!(first.proposal, later.proposal);
        let intent = super::StoredBatchIntent {
            event: first.clone(),
        };
        validate_stored_batch_intent(&record, &binding, &intent)
            .expect("validate durable batch intent");
        let mut tampered = intent.clone();
        tampered.event.identity.batch_id = ChangeBatchId(format!("sha256:{}", "f".repeat(64)));
        assert!(validate_stored_batch_intent(&record, &binding, &tampered).is_err());

        let durable_root = std::env::temp_dir().join(format!(
            "winwincode-delegated-intent-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .expect("system time")
                .as_nanos()
        ));
        let durable_store = AdapterStore::open(&durable_root).expect("open durable intent store");
        record.batch_intent = Some(intent);
        durable_store
            .save_run("delegated-run", &record)
            .expect("persist one batch intent");
        let replayed = load_stored_run(&durable_store, "delegated-run")
            .expect("load batch intent")
            .expect("stored batch intent");
        assert_eq!(replayed.batch_intent.expect("replayed intent").event, first);
        std::fs::remove_dir_all(durable_root).expect("remove durable intent store");

        let mut unknown = output.clone();
        unknown["unexpected"] = serde_json::json!(true);
        assert!(
            delegated_change_batch_event(
                &record,
                &binding,
                "turn-fixture",
                Some(&unknown.to_string()),
                &occurred_at,
            )
            .is_err()
        );
        let mut foreign_criterion = output;
        foreign_criterion["acceptanceCriteriaIds"] = serde_json::json!(["criterion-foreign"]);
        assert!(
            delegated_change_batch_event(
                &record,
                &binding,
                "turn-fixture",
                Some(&foreign_criterion.to_string()),
                &occurred_at,
            )
            .is_err()
        );
    }

    #[test]
    fn delegated_proposal_covers_the_exact_task_acceptance_set() {
        let (mut record, binding) = delegated_record_and_binding();
        record
            .job
            .stage_input
            .as_mut()
            .and_then(|input| input.task.as_mut())
            .expect("delegated task")
            .acceptance_criterion_ids
            .push("criterion-second".to_owned());
        let incomplete = serde_json::json!({
            "acceptanceCriteriaIds": ["criterion-fixture"],
            "disposition": "final",
            "patch": "*** Begin Patch\n*** Update File: src/lib.rs\n@@\n-old\n+new\n*** End Patch\n",
            "schemaVersion": 1,
            "validationProfile": "changed"
        });
        assert!(
            delegated_change_batch_event(
                &record,
                &binding,
                "turn-fixture",
                Some(&incomplete.to_string()),
                &Instant("2026-08-28T00:05:00Z".to_owned()),
            )
            .is_err()
        );
    }

    #[test]
    fn delegated_patch_paths_stay_below_the_candidate_root() {
        let multibyte = "é".repeat(262_145);
        assert!(multibyte.chars().count() <= 524_288);
        assert!(multibyte.len() > 524_288);
        assert!(validate_delegated_patch(&multibyte).is_err());

        for path in ["", "/tmp/escape", "../escape", ".", "src/../escape"] {
            assert!(
                validate_delegated_patch_path(std::path::Path::new(path)).is_err(),
                "reject {path:?}"
            );
        }
        for path in ["src/lib.rs", "nested/path/file-name.rs"] {
            validate_delegated_patch_path(std::path::Path::new(path))
                .expect("accept normal relative path");
        }

        let (record, binding) = delegated_record_and_binding();
        let moved_outside = serde_json::json!({
            "acceptanceCriteriaIds": ["criterion-fixture"],
            "disposition": "final",
            "patch": "*** Begin Patch\n*** Update File: src/lib.rs\n*** Move to: ../escape.rs\n@@\n-old\n+new\n*** End Patch\n",
            "schemaVersion": 1,
            "validationProfile": "changed"
        });
        assert!(
            delegated_change_batch_event(
                &record,
                &binding,
                "turn-fixture",
                Some(&moved_outside.to_string()),
                &Instant("2026-08-28T00:05:00Z".to_owned()),
            )
            .is_err()
        );

        let patch_with_files = |count: usize| {
            let mut patch = String::from("*** Begin Patch\n");
            for index in 0..count {
                writeln!(patch, "*** Delete File: src/file-{index}.rs")
                    .expect("write patch fixture");
            }
            patch.push_str("*** End Patch\n");
            patch
        };
        validate_delegated_patch(&patch_with_files(20)).expect("accept twenty-file plan");
        assert!(validate_delegated_patch(&patch_with_files(21)).is_err());

        let patch_with_hunks = |count: usize| {
            let mut patch = String::from("*** Begin Patch\n");
            for _ in 0..count {
                patch.push_str("*** Delete File: src/repeated.rs\n");
            }
            patch.push_str("*** End Patch\n");
            patch
        };
        validate_delegated_patch(&patch_with_hunks(100)).expect("accept one hundred hunks");
        assert!(validate_delegated_patch(&patch_with_hunks(101)).is_err());
    }

    #[cfg(unix)]
    #[test]
    fn forged_public_handshake_outside_release_layout_is_rejected() {
        use std::os::unix::fs::PermissionsExt as _;

        let root =
            std::env::temp_dir().join(format!("winwincode-renamed-helper-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).expect("create helper fixture");
        let helper = root.join("winwincode-kernel-helper");
        std::fs::write(
            &helper,
            format!(
                "#!/bin/sh\nprintf '%s\\n' '{{\"protocol\":\"winwincode-kernel-helper\",\"version\":1,\"packageVersion\":\"{}\"}}'\n",
                env!("CARGO_PKG_VERSION")
            ),
        )
        .expect("write forged executable");
        std::fs::set_permissions(&helper, std::fs::Permissions::from_mode(0o755))
            .expect("make renamed executable runnable");
        let manifest = HelperReleaseManifest::from_test_helper(&helper)
            .expect("build test helper release manifest");
        assert!(project_helper(&PathBuf::from(&helper), &manifest).is_none());
        std::fs::remove_dir_all(root).expect("remove helper fixture");
    }

    #[cfg(unix)]
    #[test]
    fn sealed_helper_is_atomic_private_and_repairs_crash_snapshot_mode() {
        use std::os::unix::fs::PermissionsExt as _;

        let root = test_root("seal");
        std::fs::create_dir_all(&root).expect("create helper fixture");
        let helper = helper_fixture(&root);
        let manifest = HelperReleaseManifest::from_test_helper(&helper)
            .expect("build signed helper fixture manifest");
        assert_eq!(manifest.binary_path(), "winwincode-kernel-helper");
        assert_eq!(manifest.binary_mode(), 0o755);
        let data = root.join("runtime");
        let sealed = seal_helper(&helper, None, &data, &manifest).expect("seal helper");
        assert_eq!(
            sealed,
            data.join("helper-installation/winwincode-kernel-helper")
        );
        assert_eq!(
            std::fs::metadata(data.join("helper-installation"))
                .expect("installation directory metadata")
                .permissions()
                .mode()
                & 0o777,
            0o700
        );
        assert_eq!(
            std::fs::metadata(&sealed)
                .expect("sealed helper metadata")
                .permissions()
                .mode()
                & 0o777,
            0o700
        );

        // A crash/archive restore may leave exact bytes with a stale mode.
        // Re-open repairs only that metadata after re-validating the bytes.
        std::fs::set_permissions(&sealed, std::fs::Permissions::from_mode(0o600))
            .expect("remove executable mode from crash snapshot");
        assert_eq!(
            seal_helper(&helper, None, &data, &manifest).expect("repair sealed helper"),
            sealed
        );
        assert_eq!(
            std::fs::metadata(&sealed)
                .expect("repaired helper metadata")
                .permissions()
                .mode()
                & 0o777,
            0o700
        );
        std::fs::remove_dir_all(root).expect("remove helper fixture");
    }

    #[cfg(unix)]
    #[test]
    fn validated_helper_bytes_survive_release_path_replacement_before_sealing() {
        use std::os::unix::fs::PermissionsExt as _;

        let root = test_root("validated-replacement");
        std::fs::create_dir_all(&root).expect("create helper fixture");
        let helper = helper_fixture(&root);
        let manifest = HelperReleaseManifest::from_test_helper(&helper)
            .expect("build signed helper fixture manifest");
        let validated = read_helper_bytes(&helper).expect("read validated helper bytes");
        std::fs::write(&helper, b"#!/bin/sh\nexit 0\n").expect("replace release helper bytes");
        std::fs::set_permissions(&helper, std::fs::Permissions::from_mode(0o600))
            .expect("make replacement release path non-executable");

        let data = root.join("runtime");
        let sealed = seal_helper(&helper, Some(&validated), &data, &manifest)
            .expect("seal the exact bytes owned by validated configuration");
        assert_eq!(
            read_helper_bytes(&sealed).expect("read sealed helper"),
            validated
        );
        assert!(validate_sealed_helper(&sealed, &manifest));
        std::fs::remove_dir_all(root).expect("remove helper fixture");
    }

    #[cfg(unix)]
    #[test]
    fn sealed_helper_rejects_source_replacement_and_destination_symlink() {
        use std::os::unix::fs::{PermissionsExt as _, symlink};

        let root = test_root("replacement");
        std::fs::create_dir_all(&root).expect("create helper fixture");
        let helper = helper_fixture(&root);
        let manifest = HelperReleaseManifest::from_test_helper(&helper)
            .expect("build signed helper fixture manifest");
        let replacement_data = root.join("replacement-runtime");
        std::fs::write(&helper, b"#!/bin/sh\nexit 0\n").expect("replace helper bytes");
        std::fs::set_permissions(&helper, std::fs::Permissions::from_mode(0o755))
            .expect("restore replacement helper mode");
        assert_eq!(
            seal_helper(&helper, None, &replacement_data, &manifest)
                .expect_err("changed helper must fail closed")
                .kind(),
            ProductionCodexErrorKind::InvalidConfiguration
        );

        let symlink_data = root.join("symlink-runtime");
        let destination_directory = symlink_data.join("helper-installation");
        std::fs::create_dir_all(&destination_directory).expect("create symlink destination");
        std::fs::set_permissions(
            &destination_directory,
            std::fs::Permissions::from_mode(0o700),
        )
        .expect("restrict symlink destination");
        let destination = destination_directory.join("winwincode-kernel-helper");
        symlink(&helper, &destination).expect("create replacement symlink");
        assert_eq!(
            seal_helper(&helper, None, &symlink_data, &manifest)
                .expect_err("destination symlink must fail closed")
                .kind(),
            ProductionCodexErrorKind::InvalidConfiguration
        );
        std::fs::remove_dir_all(root).expect("remove helper fixture");
    }

    #[cfg(unix)]
    #[test]
    fn helper_handshake_succeeds_without_waiting_for_the_cleanup_deadline() {
        use std::os::unix::fs::PermissionsExt as _;

        let root = test_root("normal-handshake");
        std::fs::create_dir_all(&root).expect("create helper fixture");
        let helper = root.join("winwincode-kernel-helper");
        let expected = b"exact helper handshake\n";
        std::fs::write(&helper, "#!/bin/sh\nprintf 'exact helper handshake\\n'\n")
            .expect("write normal helper");
        std::fs::set_permissions(&helper, std::fs::Permissions::from_mode(0o755))
            .expect("make normal helper runnable");

        let started = std::time::Instant::now();
        assert!(bounded_helper_handshake(&helper, expected));
        assert!(started.elapsed() < std::time::Duration::from_secs(1));
        std::fs::remove_dir_all(root).expect("remove helper fixture");
    }

    #[cfg(unix)]
    #[test]
    fn concurrent_release_probes_share_one_bounded_process_window() {
        use std::os::unix::fs::PermissionsExt as _;
        use std::sync::{Arc, Barrier};

        const CONCURRENCY: usize = 8;
        let root = test_root("concurrent-release-probes");
        std::fs::create_dir_all(&root).expect("create helper fixture");
        let helper = root.join("winwincode-kernel-helper");
        let active = root.join("active");
        std::fs::write(
            &helper,
            format!(
                "#!/bin/sh\nmkdir '{}' || exit 9\ntrap 'rmdir \"{}\"' EXIT\nsleep 0.05\ncase \"$1\" in\n  --winwincode-helper-handshake) printf 'exact helper handshake\\n' ;;\n  --winwincode-helper-identity) printf 'exact helper identity\\n' ;;\n  *) exit 2 ;;\nesac\n",
                active.display(),
                active.display(),
            ),
        )
        .expect("write contention-sensitive helper");
        std::fs::set_permissions(&helper, std::fs::Permissions::from_mode(0o755))
            .expect("make helper executable");
        let manifest = Arc::new(
            HelperReleaseManifest::from_test_helper(&helper).expect("build helper manifest"),
        );

        let helper = Arc::new(helper);
        let barrier = Arc::new(Barrier::new(CONCURRENCY + 1));
        let threads = (0..CONCURRENCY)
            .map(|_| {
                let helper = Arc::clone(&helper);
                let manifest = Arc::clone(&manifest);
                let barrier = Arc::clone(&barrier);
                std::thread::spawn(move || {
                    barrier.wait();
                    validate_helper_image(
                        &helper,
                        &manifest,
                        b"exact helper handshake\n",
                        b"exact helper identity\n",
                    )
                    .is_some()
                })
            })
            .collect::<Vec<_>>();
        barrier.wait();
        assert!(
            threads
                .into_iter()
                .all(|thread| thread.join().expect("join release probe"))
        );
        assert!(!active.exists());
        std::fs::remove_dir_all(root).expect("remove helper fixture");
    }

    #[cfg(unix)]
    #[test]
    fn cold_release_probe_retries_once_without_weakening_the_exact_identity() {
        use std::os::unix::fs::PermissionsExt as _;

        let root = test_root("cold-release-probe");
        std::fs::create_dir_all(&root).expect("create helper fixture");
        let helper = root.join("winwincode-kernel-helper");
        let first_probe = root.join("first-probe");
        std::fs::write(
            &helper,
            format!(
                "#!/bin/sh\nif [ \"$1\" = '--winwincode-helper-handshake' ] && [ ! -e '{}' ]; then\n  : > '{}'\n  exit 9\nfi\ncase \"$1\" in\n  --winwincode-helper-handshake) printf 'exact helper handshake\\n' ;;\n  --winwincode-helper-identity) printf 'exact helper identity\\n' ;;\n  *) exit 2 ;;\nesac\n",
                first_probe.display(),
                first_probe.display(),
            ),
        )
        .expect("write cold-start helper");
        std::fs::set_permissions(&helper, std::fs::Permissions::from_mode(0o755))
            .expect("make helper executable");
        let manifest =
            HelperReleaseManifest::from_test_helper(&helper).expect("build helper manifest");

        assert!(
            validate_helper_image(
                &helper,
                &manifest,
                b"exact helper handshake\n",
                b"exact helper identity\n",
            )
            .is_some()
        );
        assert!(first_probe.is_file());
        std::fs::remove_dir_all(root).expect("remove helper fixture");
    }

    #[cfg(unix)]
    #[test]
    fn helper_handshake_has_a_bounded_timeout() {
        use std::os::unix::fs::PermissionsExt as _;

        let root = test_root("sleeping-helper");
        std::fs::create_dir_all(&root).expect("create helper fixture");
        let helper = root.join("winwincode-kernel-helper");
        let process_id_path = root.join("helper.pid");
        std::fs::write(
            &helper,
            format!(
                "#!/bin/sh\nprintf '%s' \"$$\" > '{}'\nsleep 999\n",
                process_id_path.display()
            ),
        )
        .expect("write sleeping helper");
        std::fs::set_permissions(&helper, std::fs::Permissions::from_mode(0o755))
            .expect("make sleeping helper runnable");
        let started = std::time::Instant::now();
        assert!(!bounded_helper_handshake(&helper, b"never"));
        assert!(started.elapsed() < std::time::Duration::from_secs(4));
        let process_id = std::fs::read_to_string(process_id_path)
            .expect("read helper process id")
            .parse::<u32>()
            .expect("parse helper process id");
        assert_process_stops(process_id);
        std::fs::remove_dir_all(root).expect("remove helper fixture");
    }

    #[cfg(unix)]
    #[test]
    fn helper_handshake_closes_inherited_descendant_pipes() {
        use std::os::unix::fs::PermissionsExt as _;

        let root = test_root("descendant-helper");
        std::fs::create_dir_all(&root).expect("create helper fixture");
        let helper = root.join("winwincode-kernel-helper");
        let helper_process_id_path = root.join("helper.pid");
        let descendant_process_id_path = root.join("descendant.pid");
        let expected = b"exact helper handshake\n";
        std::fs::write(
            &helper,
            format!(
                "#!/bin/sh\nprintf '%s' \"$$\" > '{}'\nsleep 999 &\nprintf '%s' \"$!\" > '{}'\nprintf 'exact helper handshake\\n'\n",
                helper_process_id_path.display(),
                descendant_process_id_path.display()
            ),
        )
        .expect("write descendant helper");
        std::fs::set_permissions(&helper, std::fs::Permissions::from_mode(0o755))
            .expect("make descendant helper runnable");
        let started = std::time::Instant::now();
        assert!(bounded_helper_handshake(&helper, expected));
        assert!(started.elapsed() < std::time::Duration::from_secs(4));
        let helper_process_id = std::fs::read_to_string(helper_process_id_path)
            .expect("read helper process id")
            .parse::<u32>()
            .expect("parse helper process id");
        let descendant_process_id = std::fs::read_to_string(descendant_process_id_path)
            .expect("read descendant process id")
            .parse::<u32>()
            .expect("parse descendant process id");
        assert_process_stops(helper_process_id);
        assert_process_stops(descendant_process_id);
        std::fs::remove_dir_all(root).expect("remove helper fixture");
    }

    #[cfg(unix)]
    #[test]
    fn helper_timeout_does_not_terminate_a_foreign_process_group() {
        use std::os::unix::fs::PermissionsExt as _;
        use std::os::unix::process::CommandExt as _;

        let mut foreign = std::process::Command::new("/bin/sleep");
        foreign
            .arg("999")
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .process_group(0);
        let mut foreign = foreign.spawn().expect("spawn foreign process group");

        let root = test_root("foreign-process-group");
        std::fs::create_dir_all(&root).expect("create helper fixture");
        let helper = root.join("winwincode-kernel-helper");
        std::fs::write(&helper, "#!/bin/sh\nsleep 999\n").expect("write sleeping helper");
        std::fs::set_permissions(&helper, std::fs::Permissions::from_mode(0o755))
            .expect("make sleeping helper runnable");

        assert!(!bounded_helper_handshake(&helper, b"never"));
        assert!(foreign.try_wait().expect("query foreign process").is_none());
        assert!(terminate_helper_process_group(
            &mut foreign,
            std::time::Duration::from_secs(1)
        ));
        std::fs::remove_dir_all(root).expect("remove helper fixture");
    }
}
