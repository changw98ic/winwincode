// SPDX-License-Identifier: Apache-2.0

//! Durable, host-admitted scheduling for one bounded `DebugProbe` round.
//!
//! This module deliberately stops at the round boundary. It has no Codex
//! adapter and exposes no per-probe feedback callback, so an individual probe
//! completion cannot schedule a model turn. The future D9 composition may
//! submit the single terminal [`ProbeRoundReceipt`] returned by
//! [`ProbeScheduler::run_round`].

use std::{
    collections::{BTreeMap, BTreeSet},
    fmt, fs,
    future::Future,
    path::{Component, Path, PathBuf},
    pin::Pin,
    sync::atomic::{AtomicU8, Ordering},
    sync::{Arc, Mutex},
    task::Poll,
    time::{Duration, Instant as MonotonicInstant, SystemTime, UNIX_EPOCH},
};

use rusqlite::{Connection, OptionalExtension as _, params};
use winwincode_domain::Instant;
use winwincode_execution_port::{
    debug_probe_contract::{
        ValidatedDebugProbePlan, ValidatedProbeExecutionIntent, seal_debug_probe_plan,
        seal_probe_execution_intent, validate_probe_execution_receipt,
        validate_probe_round_receipt,
    },
    generated::{
        ArtifactReference, DebugProbeError, DebugProbeErrorCode, DebugProbeKind, DebugProbePlan,
        DebugProbeRoundAuthority, HypothesisEvidenceCandidate, ProbeCommandSpec,
        ProbeCompletionRuleKind, ProbeEvidenceSummary, ProbeExecutionIntent, ProbeExecutionReceipt,
        ProbeNormalizerProfile, ProbeReceiptStatus, ProbeResourceClaim, ProbeRoundBudgetUsage,
        ProbeRoundCompletionReason, ProbeRoundReceipt, ProbeRoundReceiptStatus,
        ProbeSideEffectClass, ProbeWorkspaceAccess,
    },
    probe_result_normalizer::{
        ProbeEvidenceProjection, ProbeRawStreamInput, ValidatedProbeEvidenceBundle,
        ValidatedProbeNormalizerProfile, normalize_probe_evidence, seal_probe_normalizer_profile,
    },
};

use crate::probe_evidence::{
    DurableProbeEvidenceStore, PreparedProbeCapture, ProbeEvidenceStoreError,
    ProbeEvidenceStoreErrorKind, ProbeNormalizationContext, ProbePriorEvidenceReference,
    ProbeRawArtifactManifest, ProbeRawArtifactReadRequest, RecoveredProbeEvidence,
    RetainedProbeEvidenceBundle,
};

const DATABASE_DIRECTORY: &str = ".probe-scheduler";
const DATABASE_FILE: &str = "probe-scheduler.sqlite3";
const EVIDENCE_DIRECTORY: &str = ".probe-evidence";
const SCHEMA: &str = "
CREATE TABLE IF NOT EXISTS probe_round (
    round_id TEXT PRIMARY KEY,
    plan_json BLOB NOT NULL,
    receipt_json BLOB,
    workspace_root TEXT NOT NULL,
    workspace_request_root TEXT NOT NULL,
    observed_elapsed_millis INTEGER NOT NULL,
    stop_reason TEXT,
    stop_elapsed_millis INTEGER,
    created_at TEXT NOT NULL,
    updated_at TEXT NOT NULL,
    CHECK ((stop_reason IS NULL) = (stop_elapsed_millis IS NULL))
);
CREATE TABLE IF NOT EXISTS probe_execution (
    probe_execution_id TEXT PRIMARY KEY,
    round_id TEXT NOT NULL,
    ordinal INTEGER NOT NULL,
    intent_json BLOB NOT NULL,
    receipt_json BLOB,
    created_at TEXT NOT NULL,
    updated_at TEXT NOT NULL,
    UNIQUE (round_id, ordinal),
    FOREIGN KEY (round_id) REFERENCES probe_round(round_id)
);
CREATE INDEX IF NOT EXISTS probe_execution_round
    ON probe_execution (round_id, ordinal);
";

/// Opaque cancellation capability passed only to an already-admitted runner.
#[derive(Clone, Debug, Default)]
pub struct ProbeRunCancellation(Arc<AtomicU8>);

impl ProbeRunCancellation {
    /// Returns a fresh, independent cancellation capability.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Requests cooperative termination and cleanup.
    pub fn cancel(&self) {
        let _ = self
            .0
            .compare_exchange(0, 1, Ordering::AcqRel, Ordering::Acquire);
    }

    /// Reports whether termination has been requested.
    #[must_use]
    pub fn is_cancelled(&self) -> bool {
        self.0.load(Ordering::Acquire) != 0
    }

    fn exhaust_budget(&self) {
        let _ = self
            .0
            .compare_exchange(0, 2, Ordering::AcqRel, Ordering::Acquire);
    }

    fn is_budget_exhausted(&self) -> bool {
        self.0.load(Ordering::Acquire) == 2
    }
}

/// Host clock used to make every durable time fact explicit and testable.
pub trait ProbeClock: fmt::Debug + Send + Sync + 'static {
    /// Returns a canonical UTC instant.
    ///
    /// # Errors
    ///
    /// Returns a bounded scheduler error when the host clock is unavailable.
    fn now(&self) -> Result<Instant, ProbeSchedulerError>;
}

/// System UTC clock used by the released scheduler constructor.
#[derive(Clone, Copy, Debug, Default)]
pub struct SystemProbeClock;

impl ProbeClock for SystemProbeClock {
    fn now(&self) -> Result<Instant, ProbeSchedulerError> {
        system_instant()
    }
}

/// Stable failure returned before any untrusted command can start.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ProbeSchedulerError {
    code: DebugProbeErrorCode,
    message: &'static str,
}

impl ProbeSchedulerError {
    const fn new(code: DebugProbeErrorCode, message: &'static str) -> Self {
        Self { code, message }
    }

    /// Creates a bounded failure reported by an injected host runner or clock.
    #[must_use]
    pub const fn host_failure(code: DebugProbeErrorCode, message: &'static str) -> Self {
        Self::new(code, message)
    }

    /// Returns the canonical `DebugProbe` failure category.
    #[must_use]
    pub const fn code(&self) -> &DebugProbeErrorCode {
        &self.code
    }

    /// Returns a bounded message which never contains command output.
    #[must_use]
    pub const fn message(&self) -> &'static str {
        self.message
    }
}

impl fmt::Display for ProbeSchedulerError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.message)
    }
}

impl std::error::Error for ProbeSchedulerError {}

/// One host-owned exact command template.
///
/// Matching uses the full executable, argv, working directory, kind, resource
/// claim and per-probe limits. Registering `cargo`, `git`, or an interpreter
/// therefore does not authorize any other arguments.
#[derive(Clone, Debug, PartialEq)]
pub struct TrustedPureReadTemplate {
    kind: DebugProbeKind,
    command: ProbeCommandSpec,
    resources: ProbeResourceClaim,
    max_timeout_millis: i64,
    max_output_limit_bytes: i64,
    normalizer_profile: ValidatedProbeNormalizerProfile,
    prior_evidence: Option<ProbePriorEvidence>,
}

impl TrustedPureReadTemplate {
    /// Seals one exact host template. Network, database, service and mutable
    /// workspace capabilities have no released D2 template form.
    ///
    /// # Errors
    ///
    /// Rejects a non-PureRead claim, an unbounded command, non-canonical
    /// resources, or a capability whose containment is deferred to D6/D9.
    pub fn try_new(
        kind: DebugProbeKind,
        mut command: ProbeCommandSpec,
        mut resources: ProbeResourceClaim,
        max_timeout_millis: i64,
        max_output_limit_bytes: i64,
        normalizer_profile: ProbeNormalizerProfile,
        prior_evidence: Option<ProbePriorEvidence>,
    ) -> Result<Self, ProbeSchedulerError> {
        if command.argv.is_empty()
            || max_timeout_millis <= 0
            || max_output_limit_bytes <= 0
            || resources.side_effect_class != ProbeSideEffectClass::PureRead
            || resources.workspace_access != ProbeWorkspaceAccess::ReadOnly
            || !matches!(
                resources.network_access,
                winwincode_execution_port::generated::ProbeNetworkAccess::None
            )
            || !resources.database_keys.is_empty()
            || !resources.service_keys.is_empty()
        {
            return Err(invalid_probe(
                "host template exceeds the released PureRead boundary",
            ));
        }
        command.command_arg_bytes =
            winwincode_execution_port::debug_probe_contract::derive_probe_command_arg_bytes(
                &command.argv,
            )
            .map_err(|_| invalid_probe("host template command is invalid"))?;
        validate_lexical_path(&command.working_directory)?;
        canonicalize_claim_lists(&mut resources)?;
        for path in &resources.paths {
            validate_lexical_path(path)?;
        }
        let normalizer_profile = seal_probe_normalizer_profile(normalizer_profile)
            .map_err(|error| ProbeSchedulerError::new(error.code().clone(), error.message()))?;
        Ok(Self {
            kind,
            command,
            resources,
            max_timeout_millis,
            max_output_limit_bytes,
            normalizer_profile,
            prior_evidence,
        })
    }

    fn admits(&self, spec: &winwincode_execution_port::generated::ProbeSpec) -> bool {
        self.kind == spec.kind
            && self.command == spec.command
            && spec.timeout_millis <= self.max_timeout_millis
            && spec.output_limit_bytes <= self.max_output_limit_bytes
    }
}

/// Opaque exact predecessor selected by durable host round context.
///
/// Only evidence returned by [`ProbeScheduler::evidence_for_round`] can create
/// this value. A model cannot submit an Artifact reference as a baseline.
#[derive(Clone, Debug, PartialEq)]
pub struct ProbePriorEvidence {
    reference: ProbePriorEvidenceReference,
}

/// Durable normalized evidence exposed at the round boundary without raw
/// process output.
#[derive(Clone, Debug, PartialEq)]
pub struct ProbeEvidenceRecord {
    bundle_artifact_ref: ArtifactReference,
    bundle: ValidatedProbeEvidenceBundle,
    projection: ProbeEvidenceProjection,
    prior: ProbePriorEvidence,
}

impl ProbeEvidenceRecord {
    fn from_retained(retained: &RetainedProbeEvidenceBundle) -> Self {
        Self {
            bundle_artifact_ref: retained.bundle_artifact_ref().clone(),
            bundle: retained.bundle().clone(),
            projection: retained.projection().clone(),
            prior: ProbePriorEvidence {
                reference: retained.prior_reference(),
            },
        }
    }

    /// Canonical bundle Artifact reference, separate from raw receipt refs.
    #[must_use]
    pub const fn bundle_artifact_ref(&self) -> &ArtifactReference {
        &self.bundle_artifact_ref
    }

    /// Fully revalidated canonical bundle for deterministic host composition.
    #[must_use]
    pub const fn bundle(&self) -> &ValidatedProbeEvidenceBundle {
        &self.bundle
    }

    /// Bounded summary containing no raw process bytes.
    #[must_use]
    pub const fn summary(&self) -> &ProbeEvidenceSummary {
        self.projection.summary()
    }

    /// Unpolarized hypothesis candidates containing no confidence assignment.
    #[must_use]
    pub fn candidates(&self) -> &[HypothesisEvidenceCandidate] {
        self.projection.candidates()
    }

    /// Exact predecessor capability that a host may bind to a later template.
    #[must_use]
    pub fn prior_evidence(&self) -> ProbePriorEvidence {
        self.prior.clone()
    }
}

/// Host-admitted input given to the narrow process adapter.
#[derive(Clone, Debug)]
pub struct AdmittedProbe {
    intent: ValidatedProbeExecutionIntent,
    normalization: ProbeNormalizationContext,
    host_resources: ProbeResourceClaim,
    workspace_root: PathBuf,
    working_directory: PathBuf,
}

impl AdmittedProbe {
    /// Exact durable intent retained before this value reaches a runner.
    #[must_use]
    pub const fn intent(&self) -> &ProbeExecutionIntent {
        self.intent.intent()
    }

    /// Canonical resource claim recomputed from the exact host template.
    #[must_use]
    pub const fn host_resources(&self) -> &ProbeResourceClaim {
        &self.host_resources
    }

    /// Canonical read-only checkout root supplied by the Worker workspace owner.
    #[must_use]
    pub fn workspace_root(&self) -> &Path {
        &self.workspace_root
    }

    /// Canonical host-verified working directory within that checkout.
    #[must_use]
    pub fn working_directory(&self) -> &Path {
        &self.working_directory
    }
}

/// Process facts supplied while raw bytes are still inside the runner seam.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ProbeRunFacts {
    termination: ProbeRunTermination,
    exit_code: Option<i64>,
    signal: Option<String>,
    duration: Duration,
}

impl ProbeRunFacts {
    /// Creates exact terminal process facts. Raw bytes are submitted separately
    /// to [`ProbeExecutionCompletion::retain`].
    #[must_use]
    pub const fn new(
        termination: ProbeRunTermination,
        exit_code: Option<i64>,
        signal: Option<String>,
        duration: Duration,
    ) -> Self {
        Self {
            termination,
            exit_code,
            signal,
            duration,
        }
    }
}

/// Opaque durable process result consumed by the scheduler. It contains only
/// the exact receipt and private Artifact metadata, never raw output bytes.
#[derive(Clone, Debug, PartialEq)]
pub struct ProbeRunResult {
    receipt: ProbeExecutionReceipt,
    manifest: ProbeRawArtifactManifest,
}

/// Closed process result understood by the scheduler.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ProbeRunTermination {
    Exited,
    TimedOut,
    Cancelled,
    OutputLimitExceeded,
    InfrastructureError,
    CleanupFailed,
}

impl ProbeRunResult {
    /// Exact durable receipt whose Artifact refs address only raw streams.
    #[must_use]
    pub const fn receipt(&self) -> &ProbeExecutionReceipt {
        &self.receipt
    }
}

/// One-shot durable completion authority passed to an admitted runner.
///
/// The runner must call [`Self::retain`] before raw stdout or stderr leaves its
/// private seam. Capture, exact terminal receipt, host-selected profile, and
/// baseline selection are committed together.
#[derive(Clone)]
pub struct ProbeExecutionCompletion {
    journal: Arc<Mutex<ProbeJournal>>,
    evidence: Arc<DurableProbeEvidenceStore>,
    intent: ValidatedProbeExecutionIntent,
    normalization: ProbeNormalizationContext,
    started_at: Instant,
    round_started_monotonic: MonotonicInstant,
    round_deadline: MonotonicInstant,
    wall_limit_millis: i64,
    cancellation: ProbeRunCancellation,
    clock: Arc<dyn ProbeClock>,
}

impl fmt::Debug for ProbeExecutionCompletion {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("ProbeExecutionCompletion")
            .field("identity", &self.intent.intent().identity)
            .finish_non_exhaustive()
    }
}

impl ProbeExecutionCompletion {
    /// Atomically retains raw Artifacts, the exact terminal receipt, and the
    /// normalization binding before returning an opaque result.
    ///
    /// # Errors
    ///
    /// Rejects stale authority, changed bytes, invalid process facts, an
    /// unavailable clock, or unavailable durable storage.
    pub fn retain(
        &self,
        facts: ProbeRunFacts,
        stdout: &[u8],
        stderr: &[u8],
        output_truncated: bool,
    ) -> Result<ProbeRunResult, ProbeSchedulerError> {
        let finished_at = self.clock.now()?;
        let elapsed_millis = monotonic_elapsed_millis(self.round_started_monotonic);
        let process_cancelled = facts.termination == ProbeRunTermination::Cancelled;
        self.journal
            .lock()
            .map_err(|_| journal_error())?
            .observe_round_elapsed(
                &self.intent.intent().identity.round_id.0,
                elapsed_millis.min(self.wall_limit_millis),
                &finished_at,
            )?;
        if self.cancellation.is_budget_exhausted() {
            self.retain_stop_reason(
                RoundStopReason::BudgetExhausted,
                self.wall_limit_millis,
                &finished_at,
            )?;
        } else if self.cancellation.is_cancelled() || process_cancelled {
            self.retain_stop_reason(
                RoundStopReason::Cancelled,
                elapsed_millis.min(self.wall_limit_millis),
                &finished_at,
            )?;
        } else if MonotonicInstant::now() >= self.round_deadline {
            self.retain_stop_reason(
                RoundStopReason::BudgetExhausted,
                self.wall_limit_millis,
                &finished_at,
            )?;
            self.cancellation.exhaust_budget();
        }
        let prepared = PreparedProbeCapture::try_new(
            &self.intent,
            stdout,
            stderr,
            output_truncated,
            &self.normalization,
        )
        .map_err(|error| evidence_store_error(&error))?;
        let mut receipt = process_receipt(
            self.intent.intent(),
            facts,
            prepared.manifest(),
            &self.started_at,
            &finished_at,
        );
        apply_completion_cancellation(&mut receipt, &self.cancellation);
        let manifest = self
            .evidence
            .persist_completed_execution(&prepared, &receipt)
            .map_err(|error| evidence_store_error(&error))?;
        Ok(ProbeRunResult { receipt, manifest })
    }

    fn retain_stop_reason(
        &self,
        reason: RoundStopReason,
        elapsed_millis: i64,
        now: &Instant,
    ) -> Result<(), ProbeSchedulerError> {
        let retained = self
            .journal
            .lock()
            .map_err(|_| journal_error())?
            .retain_round_stop(
                &self.intent.intent().identity.round_id.0,
                reason,
                elapsed_millis,
                now,
            )?;
        match retained.reason {
            RoundStopReason::BudgetExhausted => self.cancellation.exhaust_budget(),
            RoundStopReason::Cancelled => self.cancellation.cancel(),
        }
        Ok(())
    }
}

/// Boxed runner future used by production and deterministic fixture adapters.
pub type ProbeRunnerFuture<'runner> =
    Pin<Box<dyn Future<Output = Result<ProbeRunResult, ProbeSchedulerError>> + Send + 'runner>>;

/// Narrow execution seam below host admission and durable intent retention.
pub trait ProbeRunner: fmt::Debug + Send + Sync + 'static {
    /// Performs adapter-specific sealing without starting a process.
    ///
    /// # Errors
    ///
    /// Rejects a command when the platform cannot prove its requested limits.
    fn validate(&self, probe: &AdmittedProbe) -> Result<(), ProbeSchedulerError>;

    /// Executes one already-admitted probe. The Scheduler calls this only after
    /// the transaction containing every round intent has committed.
    fn execute(
        &self,
        probe: AdmittedProbe,
        cancellation: ProbeRunCancellation,
        completion: ProbeExecutionCompletion,
    ) -> ProbeRunnerFuture<'_>;
}

/// Trusted authority and workspace supplied by the Worker for one model plan.
#[derive(Clone, Debug)]
pub struct ProbeRoundRequest {
    expected_authority: DebugProbeRoundAuthority,
    plan: DebugProbePlan,
    workspace_root: PathBuf,
}

impl ProbeRoundRequest {
    /// Creates a request whose expected authority is host-owned rather than
    /// copied from the model plan.
    #[must_use]
    pub fn new(
        expected_authority: DebugProbeRoundAuthority,
        plan: DebugProbePlan,
        workspace_root: impl Into<PathBuf>,
    ) -> Self {
        Self {
            expected_authority,
            plan,
            workspace_root: workspace_root.into(),
        }
    }
}

#[derive(Clone)]
struct ActiveRound {
    authority: DebugProbeRoundAuthority,
    cancellation: ProbeRunCancellation,
    started_monotonic: MonotonicInstant,
    wall_limit_millis: i64,
}

/// Deep Worker-owned facade for one complete `DebugProbe` scheduling round.
pub struct ProbeScheduler<Runner, Clock = SystemProbeClock> {
    journal: Arc<Mutex<ProbeJournal>>,
    evidence: Arc<DurableProbeEvidenceStore>,
    runner: Arc<Runner>,
    templates: Vec<TrustedPureReadTemplate>,
    active: Mutex<BTreeMap<String, ActiveRound>>,
    clock: Arc<Clock>,
}

impl<Runner, Clock> fmt::Debug for ProbeScheduler<Runner, Clock> {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("ProbeScheduler")
            .field("templates", &self.templates.len())
            .finish_non_exhaustive()
    }
}

impl<Runner> ProbeScheduler<Runner, SystemProbeClock>
where
    Runner: ProbeRunner,
{
    /// Opens the private journal and installs the complete trusted command catalog.
    ///
    /// # Errors
    ///
    /// Rejects an unavailable journal or duplicate exact command templates.
    pub fn open(
        root: impl AsRef<Path>,
        runner: Runner,
        templates: Vec<TrustedPureReadTemplate>,
    ) -> Result<Self, ProbeSchedulerError> {
        Self::open_with_clock(root, runner, templates, SystemProbeClock)
    }
}

impl<Runner, Clock> ProbeScheduler<Runner, Clock>
where
    Runner: ProbeRunner,
    Clock: ProbeClock,
{
    /// Opens the private journal with an explicit host clock.
    ///
    /// # Errors
    ///
    /// Rejects an unavailable journal or duplicate exact command templates.
    pub fn open_with_clock(
        root: impl AsRef<Path>,
        runner: Runner,
        templates: Vec<TrustedPureReadTemplate>,
        clock: Clock,
    ) -> Result<Self, ProbeSchedulerError> {
        let mut keys = BTreeSet::new();
        for template in &templates {
            let key = serde_json::to_vec(&(
                &template.kind,
                &template.command.argv,
                &template.command.working_directory,
            ))
            .map_err(|_| invalid_probe("host template cannot be encoded"))?;
            if !keys.insert(key) {
                return Err(invalid_probe("host command template is duplicated"));
            }
        }
        let root = root.as_ref();
        Ok(Self {
            journal: Arc::new(Mutex::new(ProbeJournal::open(root)?)),
            evidence: Arc::new(
                DurableProbeEvidenceStore::open(root.join(EVIDENCE_DIRECTORY))
                    .map_err(|error| evidence_store_error(&error))?,
            ),
            runner: Arc::new(runner),
            templates,
            active: Mutex::new(BTreeMap::new()),
            clock: Arc::new(clock),
        })
    }

    /// Validates, durably claims and executes one complete round.
    ///
    /// Every intent is committed in one transaction before the first runner
    /// call. Exact terminal replay returns the original receipt. An incomplete
    /// prior claim fails closed and is never executed again automatically.
    ///
    /// # Errors
    ///
    /// Rejects stale authority, changed replay bytes, unsafe host claims,
    /// unresolved restart state, or unavailable durable state.
    pub async fn run_round(
        &self,
        request: ProbeRoundRequest,
    ) -> Result<ProbeRoundReceipt, ProbeSchedulerError> {
        let validated = seal_debug_probe_plan(request.plan, &request.expected_authority)
            .map_err(|error| ProbeSchedulerError::new(error.code().clone(), error.message()))?;
        let request_workspace_root = absolute_workspace_request(&request.workspace_root)?;
        let workspace_missing = !request.workspace_root.exists();
        let replay_workspace_root = replay_workspace(&request.workspace_root)?;
        let replay = self
            .journal
            .lock()
            .map_err(|_| journal_error())?
            .replay_round(
                &validated,
                &replay_workspace_root,
                &request_workspace_root,
                workspace_missing,
            )?;
        match replay {
            RoundReplay::Terminal(receipt) => {
                self.read_evidence_for_validated(&validated, &receipt)?;
                return Ok(*receipt);
            }
            RoundReplay::Unresolved => {
                canonical_workspace(&request.workspace_root)?;
                if let Some(receipt) = self.resume_unresolved(&validated)? {
                    return Ok(receipt);
                }
                return Err(ProbeSchedulerError::new(
                    DebugProbeErrorCode::InfrastructureError,
                    "unfinished probe intent requires explicit reconciliation",
                ));
            }
            RoundReplay::Missing => {}
        }
        let workspace_root = canonical_workspace(&request.workspace_root)?;
        let round_started_monotonic = MonotonicInstant::now();
        let round_started_at = self.clock.now()?;
        let admitted = self.admit(&validated, &workspace_root, &round_started_at)?;
        for probe in &admitted {
            self.runner.validate(probe)?;
        }
        let waves = deterministic_waves(&validated, &admitted)?;
        validate_wave_budget(&validated, &admitted, &waves)?;

        let cancellation = ProbeRunCancellation::new();
        let round_key = request.expected_authority.round_id.0.clone();
        {
            let mut active = self.active.lock().map_err(|_| journal_error())?;
            if active.contains_key(&round_key) {
                return Err(resource_conflict("probe round is already active"));
            }
            let claim = self
                .journal
                .lock()
                .map_err(|_| journal_error())?
                .claim_round(
                    &validated,
                    &admitted,
                    &workspace_root,
                    &request_workspace_root,
                    &round_started_at,
                )?;
            match claim {
                RoundClaim::Terminal(receipt) => {
                    drop(active);
                    self.read_evidence_for_validated(&validated, &receipt)?;
                    return Ok(*receipt);
                }
                RoundClaim::Unresolved => {
                    return Err(ProbeSchedulerError::new(
                        DebugProbeErrorCode::InfrastructureError,
                        "unfinished probe intent requires explicit reconciliation",
                    ));
                }
                RoundClaim::Claimed => {
                    active.insert(
                        round_key.clone(),
                        ActiveRound {
                            authority: request.expected_authority.clone(),
                            cancellation: cancellation.clone(),
                            started_monotonic: round_started_monotonic,
                            wall_limit_millis: validated.plan().budget.wall_time_limit_millis,
                        },
                    );
                }
            }
        }

        let result = self
            .execute_waves(
                &validated,
                admitted,
                waves,
                cancellation,
                round_started_monotonic,
            )
            .await;
        if result.is_err() {
            self.remove_active(&round_key)?;
        }
        result
    }

    fn resume_unresolved(
        &self,
        plan: &ValidatedDebugProbePlan,
    ) -> Result<Option<ProbeRoundReceipt>, ProbeSchedulerError> {
        let (intents, recovery) = {
            let journal = self.journal.lock().map_err(|_| journal_error())?;
            (journal.load_intents(plan)?, journal.recovery_context(plan)?)
        };
        let mut receipts = Vec::with_capacity(intents.len());
        for intent in intents {
            let intent = seal_probe_execution_intent(intent, plan)
                .map_err(|error| ProbeSchedulerError::new(error.code().clone(), error.message()))?;
            let journal_receipt = self
                .journal
                .lock()
                .map_err(|_| journal_error())?
                .load_probe_receipt(intent.intent())?;
            let recovered = self
                .evidence
                .recover(&intent)
                .map_err(|error| evidence_store_error(&error))?;
            let receipt = match recovered {
                Some(RecoveredProbeEvidence::ReceiptRetained {
                    manifest,
                    receipt,
                    normalization,
                }) => {
                    self.normalize_recovered(
                        &intent,
                        &manifest,
                        &receipt,
                        &normalization,
                        &recovery.workspace_root,
                    )?;
                    receipt
                }
                Some(RecoveredProbeEvidence::Complete {
                    receipt, evidence, ..
                }) => {
                    if evidence.bundle().bundle().l0_receipt != receipt {
                        return Err(stale_authority(
                            "recovered evidence differs from its execution receipt",
                        ));
                    }
                    receipt
                }
                None => {
                    let Some(receipt) = journal_receipt.clone() else {
                        return Ok(None);
                    };
                    if !receipt.artifact_refs.is_empty() || receipt.output_bytes != 0 {
                        return Ok(None);
                    }
                    receipts.push(receipt);
                    continue;
                }
            };
            if journal_receipt
                .as_ref()
                .is_some_and(|retained| retained != &receipt)
            {
                return Err(stale_authority(
                    "recovered receipt differs from the scheduler journal",
                ));
            }
            self.journal
                .lock()
                .map_err(|_| journal_error())?
                .retain_probe_receipt(&receipt, &receipt.finished_at)?;
            receipts.push(receipt);
        }
        let receipt = canonical_round_receipt(plan, receipts, &recovery)?;
        self.journal
            .lock()
            .map_err(|_| journal_error())?
            .retain_round_receipt(&receipt, &receipt.finished_at)?;
        Ok(Some(receipt))
    }

    /// Reads the exact normalized evidence for one terminal round.
    ///
    /// Returned records contain canonical bundles, bounded summaries, and
    /// unpolarized candidates. Raw Artifact bodies remain private. A record's
    /// opaque predecessor capability may be explicitly attached to a later
    /// trusted command template.
    ///
    /// # Errors
    ///
    /// Rejects a non-terminal round, changed plan bytes, missing evidence,
    /// or any durable authority/digest mismatch.
    pub fn evidence_for_round(
        &self,
        request: &ProbeRoundRequest,
    ) -> Result<Vec<ProbeEvidenceRecord>, ProbeSchedulerError> {
        let validated = seal_debug_probe_plan(request.plan.clone(), &request.expected_authority)
            .map_err(|error| ProbeSchedulerError::new(error.code().clone(), error.message()))?;
        let request_workspace_root = absolute_workspace_request(&request.workspace_root)?;
        let workspace_missing = !request.workspace_root.exists();
        let workspace_root = replay_workspace(&request.workspace_root)?;
        let replay = self
            .journal
            .lock()
            .map_err(|_| journal_error())?
            .replay_round(
                &validated,
                &workspace_root,
                &request_workspace_root,
                workspace_missing,
            )?;
        let receipt = match replay {
            RoundReplay::Terminal(receipt) => receipt,
            RoundReplay::Missing | RoundReplay::Unresolved => {
                return Err(evidence_missing());
            }
        };
        self.read_evidence_for_validated(&validated, &receipt)
    }

    fn read_evidence_for_validated(
        &self,
        plan: &ValidatedDebugProbePlan,
        receipt: &ProbeRoundReceipt,
    ) -> Result<Vec<ProbeEvidenceRecord>, ProbeSchedulerError> {
        let intents = self
            .journal
            .lock()
            .map_err(|_| journal_error())?
            .load_intents(plan)?;
        let mut records = Vec::new();
        for intent in intents {
            let validated = seal_probe_execution_intent(intent, plan)
                .map_err(|error| ProbeSchedulerError::new(error.code().clone(), error.message()))?;
            let probe_receipt = receipt
                .probe_receipts
                .iter()
                .find(|candidate| candidate.identity == validated.intent().identity)
                .ok_or_else(journal_error)?;
            match self
                .evidence
                .read_bundle(&validated)
                .map_err(|error| evidence_store_error(&error))?
            {
                Some(evidence) => {
                    if evidence.bundle().bundle().l0_receipt != *probe_receipt {
                        return Err(stale_authority(
                            "probe evidence differs from its terminal round receipt",
                        ));
                    }
                    records.push(ProbeEvidenceRecord::from_retained(&evidence));
                }
                None if probe_receipt.artifact_refs.is_empty()
                    && probe_receipt.output_bytes == 0 => {}
                None => return Err(evidence_missing()),
            }
        }
        Ok(records)
    }

    fn remove_active(&self, round_key: &str) -> Result<(), ProbeSchedulerError> {
        self.active
            .lock()
            .map_err(|_| journal_error())?
            .remove(round_key);
        Ok(())
    }

    /// Cancels only the exact currently active round authority.
    ///
    /// # Errors
    ///
    /// Rejects an unknown or stale authority without touching another round.
    pub fn cancel_round(
        &self,
        authority: &DebugProbeRoundAuthority,
    ) -> Result<(), ProbeSchedulerError> {
        let active = self.active.lock().map_err(|_| journal_error())?;
        let Some(round) = active.get(&authority.round_id.0) else {
            return Err(stale_authority("probe round is not active"));
        };
        if round.authority != *authority {
            return Err(stale_authority("probe cancellation authority is stale"));
        }
        let now = self.clock.now()?;
        let elapsed_millis = monotonic_elapsed_millis(round.started_monotonic);
        let (reason, elapsed_millis) = if elapsed_millis >= round.wall_limit_millis {
            (RoundStopReason::BudgetExhausted, round.wall_limit_millis)
        } else {
            (RoundStopReason::Cancelled, elapsed_millis)
        };
        let retained = self
            .journal
            .lock()
            .map_err(|_| journal_error())?
            .retain_round_stop(&authority.round_id.0, reason, elapsed_millis, &now)?;
        match retained.reason {
            RoundStopReason::Cancelled => round.cancellation.cancel(),
            RoundStopReason::BudgetExhausted => round.cancellation.exhaust_budget(),
        }
        Ok(())
    }

    fn completion(
        &self,
        probe: &AdmittedProbe,
        started_at: &Instant,
        round_started_monotonic: MonotonicInstant,
        round_deadline: MonotonicInstant,
        wall_limit_millis: i64,
        cancellation: &ProbeRunCancellation,
    ) -> ProbeExecutionCompletion {
        let clock: Arc<dyn ProbeClock> = self.clock.clone();
        ProbeExecutionCompletion {
            journal: Arc::clone(&self.journal),
            evidence: Arc::clone(&self.evidence),
            intent: probe.intent.clone(),
            normalization: probe.normalization.clone(),
            started_at: started_at.clone(),
            round_started_monotonic,
            round_deadline,
            wall_limit_millis,
            cancellation: cancellation.clone(),
            clock,
        }
    }

    fn normalize_retained(
        &self,
        probe: &AdmittedProbe,
        result: &ProbeRunResult,
    ) -> Result<RetainedProbeEvidenceBundle, ProbeSchedulerError> {
        let recovered = self
            .evidence
            .recover(&probe.intent)
            .map_err(|error| evidence_store_error(&error))?
            .ok_or_else(evidence_missing)?;
        match recovered {
            RecoveredProbeEvidence::ReceiptRetained {
                manifest,
                receipt,
                normalization,
            } => {
                if manifest != result.manifest || receipt != result.receipt {
                    return Err(stale_authority(
                        "retained probe completion differs from runner result",
                    ));
                }
                self.normalize_recovered(
                    &probe.intent,
                    &manifest,
                    &receipt,
                    &normalization,
                    &probe.workspace_root,
                )
            }
            RecoveredProbeEvidence::Complete {
                manifest,
                receipt,
                evidence,
                ..
            } => {
                if manifest != result.manifest || receipt != result.receipt {
                    return Err(stale_authority(
                        "retained probe completion differs from runner result",
                    ));
                }
                Ok(*evidence)
            }
        }
    }

    fn normalize_recovered(
        &self,
        intent: &ValidatedProbeExecutionIntent,
        manifest: &ProbeRawArtifactManifest,
        receipt: &ProbeExecutionReceipt,
        normalization: &ProbeNormalizationContext,
        workspace_root: &Path,
    ) -> Result<RetainedProbeEvidenceBundle, ProbeSchedulerError> {
        let raw = manifest
            .streams()
            .iter()
            .map(|binding| {
                self.evidence
                    .read_raw(&ProbeRawArtifactReadRequest::new(
                        intent,
                        binding.stream.clone(),
                        &binding.artifact_ref,
                    ))
                    .map_err(|error| evidence_store_error(&error))
            })
            .collect::<Result<Vec<_>, _>>()?;
        let inputs = manifest
            .streams()
            .iter()
            .zip(&raw)
            .map(|(binding, bytes)| {
                ProbeRawStreamInput::new(
                    binding.stream.clone(),
                    binding.artifact_ref.clone(),
                    bytes,
                )
            })
            .collect::<Vec<_>>();
        let bundle = normalize_probe_evidence(
            intent,
            receipt,
            normalization.profile(),
            &inputs,
            normalization.baseline(),
            workspace_root,
        )
        .map_err(|error| ProbeSchedulerError::new(error.code().clone(), error.message()))?;
        self.evidence
            .persist_bundle(intent, manifest, receipt, &bundle)
            .map_err(|error| evidence_store_error(&error))
    }

    fn admit(
        &self,
        plan: &ValidatedDebugProbePlan,
        workspace_root: &Path,
        now: &Instant,
    ) -> Result<Vec<AdmittedProbe>, ProbeSchedulerError> {
        let mut admitted = Vec::with_capacity(plan.probes().len());
        for probe in plan.probes() {
            let spec = probe.spec();
            let template = self
                .templates
                .iter()
                .find(|template| template.admits(spec))
                .ok_or_else(|| invalid_probe("probe command or resources are not host-admitted"))?;
            let mut requested_resources = spec.resources.clone();
            canonicalize_claim_lists(&mut requested_resources)?;
            // This comparison is intentionally explicit: `template.resources`
            // is the host recomputation, while `requested_resources` is the
            // normalized model declaration.
            if requested_resources != template.resources {
                return Err(ProbeSchedulerError::new(
                    DebugProbeErrorCode::UndeclaredResource,
                    "probe resource claim differs from the host template",
                ));
            }
            validate_claim_paths(workspace_root, &template.resources)?;
            let working_directory = resolve_existing_path(
                workspace_root,
                &spec.command.working_directory,
                "probe working directory is outside the checkout",
            )?;
            if !working_directory.is_dir() {
                return Err(invalid_probe("probe working directory is not a directory"));
            }
            let identity = plan
                .probe_identity(&spec.probe_id)
                .ok_or_else(|| invalid_probe("probe identity is unavailable"))?;
            let intent = ProbeExecutionIntent {
                created_at: now.clone(),
                identity,
                plan_digest: plan.plan().plan_digest.clone(),
                schema_version: 1,
                spec: spec.clone(),
            };
            let intent = seal_probe_execution_intent(intent, plan)
                .map_err(|error| ProbeSchedulerError::new(error.code().clone(), error.message()))?;
            let normalization = self
                .evidence
                .prepare_normalization(
                    &intent,
                    &template.normalizer_profile,
                    template
                        .prior_evidence
                        .as_ref()
                        .map(|prior| &prior.reference),
                )
                .map_err(|error| evidence_store_error(&error))?;
            admitted.push(AdmittedProbe {
                intent,
                normalization,
                host_resources: template.resources.clone(),
                workspace_root: workspace_root.to_path_buf(),
                working_directory,
            });
        }
        admitted.sort_by(|left, right| {
            left.intent
                .intent()
                .identity
                .probe_id
                .0
                .cmp(&right.intent.intent().identity.probe_id.0)
        });
        Ok(admitted)
    }

    #[allow(clippy::too_many_lines)]
    async fn execute_waves(
        &self,
        plan: &ValidatedDebugProbePlan,
        admitted: Vec<AdmittedProbe>,
        waves: Vec<Vec<usize>>,
        cancellation: ProbeRunCancellation,
        round_started_monotonic: MonotonicInstant,
    ) -> Result<ProbeRoundReceipt, ProbeSchedulerError> {
        let wall_limit_millis = plan.plan().budget.wall_time_limit_millis;
        let wall_limit = Duration::from_millis(
            u64::try_from(wall_limit_millis)
                .map_err(|_| budget_error("round wall-time budget is invalid"))?,
        );
        let round_deadline = round_started_monotonic + wall_limit;
        let mut receipts = Vec::with_capacity(admitted.len());
        let mut executed = BTreeSet::new();
        let mut cancelled = false;

        for wave in waves {
            if MonotonicInstant::now() >= round_deadline {
                let now = self.clock.now()?;
                self.retain_round_stop_current(
                    plan,
                    &cancellation,
                    RoundStopReason::BudgetExhausted,
                    wall_limit_millis,
                    &now,
                )?;
            }
            if cancellation.is_cancelled() {
                cancelled = true;
                break;
            }
            let wave_started_at = self.clock.now()?;
            let futures = wave
                .iter()
                .map(|index| {
                    let completion = self.completion(
                        &admitted[*index],
                        &wave_started_at,
                        round_started_monotonic,
                        round_deadline,
                        wall_limit_millis,
                        &cancellation,
                    );
                    self.runner
                        .execute(admitted[*index].clone(), cancellation.clone(), completion)
                })
                .collect::<Vec<_>>();
            let mut joined = Box::pin(join_all(futures));
            let results = tokio::select! {
                results = &mut joined => results,
                () = tokio::time::sleep_until(tokio::time::Instant::from_std(round_deadline)) => {
                    let now = self.clock.now()?;
                    self.retain_round_stop_current(
                        plan,
                        &cancellation,
                        RoundStopReason::BudgetExhausted,
                        wall_limit_millis,
                        &now,
                    )?;
                    joined.await
                }
            };
            for (index, result) in wave.into_iter().zip(results) {
                let result = result?;
                self.normalize_retained(&admitted[index], &result)?;
                let receipt = result.receipt;
                if MonotonicInstant::now() >= round_deadline {
                    let now = self.clock.now()?;
                    self.retain_round_stop_current(
                        plan,
                        &cancellation,
                        RoundStopReason::BudgetExhausted,
                        wall_limit_millis,
                        &now,
                    )?;
                }
                self.persist_completed_probe_receipt_current(
                    plan,
                    &admitted[index].intent,
                    &receipt,
                    &cancellation,
                    &receipt.finished_at,
                )?;
                cancelled |= receipt.status == ProbeReceiptStatus::Cancelled;
                executed.insert(index);
                receipts.push(receipt);
            }
            if completion_reached(plan, &receipts) {
                break;
            }
        }

        for (index, probe) in admitted.iter().enumerate() {
            if executed.contains(&index) {
                continue;
            }
            let now = self.clock.now()?;
            cancelled |= cancellation.is_cancelled();
            let mut receipt = skipped_receipt(probe.intent.intent(), &now, cancelled);
            self.persist_probe_receipt_current(
                plan,
                probe.intent.intent(),
                &mut receipt,
                &cancellation,
                &now,
            )?;
            receipts.push(receipt);
        }
        receipts.sort_by_key(|receipt| {
            plan.probe_by_id(&receipt.identity.probe_id).map_or(
                usize::MAX,
                winwincode_execution_port::debug_probe_contract::ValidatedProbe::ordinal,
            )
        });

        if MonotonicInstant::now() >= round_deadline {
            let now = self.clock.now()?;
            self.retain_round_stop_current(
                plan,
                &cancellation,
                RoundStopReason::BudgetExhausted,
                wall_limit_millis,
                &now,
            )?;
        }
        self.finalize_round_current(plan, receipts, &cancellation)
    }

    fn persist_probe_receipt_current(
        &self,
        plan: &ValidatedDebugProbePlan,
        intent: &ProbeExecutionIntent,
        receipt: &mut ProbeExecutionReceipt,
        cancellation: &ProbeRunCancellation,
        now: &Instant,
    ) -> Result<(), ProbeSchedulerError> {
        let active = self.active.lock().map_err(|_| journal_error())?;
        let Some(round) = active.get(&plan.plan().authority.round_id.0) else {
            return Err(stale_authority(
                "probe receipt arrived after its authority ended",
            ));
        };
        if round.authority != plan.plan().authority
            || !Arc::ptr_eq(&round.cancellation.0, &cancellation.0)
        {
            return Err(stale_authority("probe receipt authority is stale"));
        }
        let cleanup_failed = receipt
            .error
            .as_ref()
            .is_some_and(|error| error.code == DebugProbeErrorCode::ProcessCleanupFailed);
        if round.cancellation.is_cancelled() && !cleanup_failed {
            receipt.status = ProbeReceiptStatus::Cancelled;
            receipt.error = Some(probe_error(
                DebugProbeErrorCode::Cancelled,
                "probe was cancelled",
            ));
            receipt.exit_code = None;
            receipt.signal = None;
            receipt.timed_out = false;
        }
        let validated_intent = seal_probe_execution_intent(intent.clone(), plan)
            .map_err(|error| ProbeSchedulerError::new(error.code().clone(), error.message()))?;
        validate_probe_execution_receipt(receipt, &validated_intent)
            .map_err(|error| ProbeSchedulerError::new(error.code().clone(), error.message()))?;
        self.journal
            .lock()
            .map_err(|_| journal_error())?
            .retain_probe_receipt(receipt, now)
    }

    fn persist_completed_probe_receipt_current(
        &self,
        plan: &ValidatedDebugProbePlan,
        intent: &ValidatedProbeExecutionIntent,
        receipt: &ProbeExecutionReceipt,
        cancellation: &ProbeRunCancellation,
        now: &Instant,
    ) -> Result<(), ProbeSchedulerError> {
        let active = self.active.lock().map_err(|_| journal_error())?;
        let Some(round) = active.get(&plan.plan().authority.round_id.0) else {
            return Err(stale_authority(
                "probe receipt arrived after its authority ended",
            ));
        };
        if round.authority != plan.plan().authority
            || !Arc::ptr_eq(&round.cancellation.0, &cancellation.0)
        {
            return Err(stale_authority("probe receipt authority is stale"));
        }
        validate_probe_execution_receipt(receipt, intent)
            .map_err(|error| ProbeSchedulerError::new(error.code().clone(), error.message()))?;
        self.journal
            .lock()
            .map_err(|_| journal_error())?
            .retain_probe_receipt(receipt, now)
    }

    fn retain_round_stop_current(
        &self,
        plan: &ValidatedDebugProbePlan,
        cancellation: &ProbeRunCancellation,
        reason: RoundStopReason,
        elapsed_millis: i64,
        now: &Instant,
    ) -> Result<(), ProbeSchedulerError> {
        let active = self.active.lock().map_err(|_| journal_error())?;
        let Some(round) = active.get(&plan.plan().authority.round_id.0) else {
            return Err(stale_authority("probe round stop authority is stale"));
        };
        if round.authority != plan.plan().authority
            || !Arc::ptr_eq(&round.cancellation.0, &cancellation.0)
        {
            return Err(stale_authority("probe round stop authority was replaced"));
        }
        let retained = self
            .journal
            .lock()
            .map_err(|_| journal_error())?
            .retain_round_stop(
                &plan.plan().authority.round_id.0,
                reason,
                elapsed_millis,
                now,
            )?;
        match retained.reason {
            RoundStopReason::BudgetExhausted => round.cancellation.exhaust_budget(),
            RoundStopReason::Cancelled => round.cancellation.cancel(),
        }
        Ok(())
    }

    fn finalize_round_current(
        &self,
        plan: &ValidatedDebugProbePlan,
        receipts: Vec<ProbeExecutionReceipt>,
        cancellation: &ProbeRunCancellation,
    ) -> Result<ProbeRoundReceipt, ProbeSchedulerError> {
        let mut active = self.active.lock().map_err(|_| journal_error())?;
        let Some(round) = active.get(&plan.plan().authority.round_id.0) else {
            return Err(stale_authority("probe round ended under stale authority"));
        };
        if round.authority != plan.plan().authority
            || !Arc::ptr_eq(&round.cancellation.0, &cancellation.0)
        {
            return Err(stale_authority("probe round authority was replaced"));
        }
        let mut journal = self.journal.lock().map_err(|_| journal_error())?;
        let recovery = journal.recovery_context(plan)?;
        let receipt = canonical_round_receipt(plan, receipts, &recovery)?;
        journal.retain_round_receipt(&receipt, &receipt.finished_at)?;
        active.remove(&plan.plan().authority.round_id.0);
        Ok(receipt)
    }
}

fn deterministic_waves(
    plan: &ValidatedDebugProbePlan,
    admitted: &[AdmittedProbe],
) -> Result<Vec<Vec<usize>>, ProbeSchedulerError> {
    let parallel_limit = usize::try_from(plan.plan().budget.parallel_probe_limit)
        .map_err(|_| budget_error("parallel probe limit is invalid"))?;
    let memory_limit = plan.plan().budget.peak_memory_limit_bytes;
    let mut waves: Vec<Vec<usize>> = Vec::new();
    for (index, probe) in admitted.iter().enumerate() {
        let memory = probe.host_resources.memory_limit_bytes;
        if memory > memory_limit {
            return Err(budget_error("one probe exceeds peak memory budget"));
        }
        let selected = waves.iter().position(|wave| {
            wave.len() < parallel_limit
                && wave
                    .iter()
                    .map(|other| admitted[*other].host_resources.memory_limit_bytes)
                    .sum::<i64>()
                    .saturating_add(memory)
                    <= memory_limit
                && wave.iter().all(|other| {
                    !claims_conflict(&admitted[*other].host_resources, &probe.host_resources)
                })
        });
        if let Some(wave) = selected {
            waves[wave].push(index);
        } else {
            waves.push(vec![index]);
        }
    }
    Ok(waves)
}

fn validate_wave_budget(
    plan: &ValidatedDebugProbePlan,
    admitted: &[AdmittedProbe],
    waves: &[Vec<usize>],
) -> Result<(), ProbeSchedulerError> {
    let worst_case_wall = waves.iter().try_fold(0_i64, |total, wave| {
        let wave_timeout = wave
            .iter()
            .map(|index| admitted[*index].intent.intent().spec.timeout_millis)
            .max()
            .unwrap_or(0);
        total.checked_add(wave_timeout)
    });
    if worst_case_wall.is_none_or(|total| total > plan.plan().budget.wall_time_limit_millis) {
        return Err(budget_error(
            "deterministic probe waves exceed the round wall-time budget",
        ));
    }
    Ok(())
}

fn claims_conflict(left: &ProbeResourceClaim, right: &ProbeResourceClaim) -> bool {
    if left.side_effect_class == ProbeSideEffectClass::Exclusive
        || right.side_effect_class == ProbeSideEffectClass::Exclusive
    {
        return true;
    }
    overlaps(&left.port_numbers, &right.port_numbers)
        || overlaps(&left.service_keys, &right.service_keys)
        || overlaps(&left.database_keys, &right.database_keys)
        || overlaps(&left.exclusive_keys, &right.exclusive_keys)
        || ((left.side_effect_class != ProbeSideEffectClass::PureRead
            || right.side_effect_class != ProbeSideEffectClass::PureRead)
            && paths_overlap(&left.paths, &right.paths))
}

fn overlaps<T: Ord>(left: &[T], right: &[T]) -> bool {
    left.iter().any(|value| right.binary_search(value).is_ok())
}

fn paths_overlap(left: &[String], right: &[String]) -> bool {
    left.iter().any(|left| {
        right.iter().any(|right| {
            left == right
                || left
                    .strip_prefix(right)
                    .is_some_and(|rest| rest.starts_with('/'))
                || right
                    .strip_prefix(left)
                    .is_some_and(|rest| rest.starts_with('/'))
        })
    })
}

fn completion_reached(plan: &ValidatedDebugProbePlan, receipts: &[ProbeExecutionReceipt]) -> bool {
    let rule = &plan.plan().completion_rule;
    let required_failed = rule.stop_on_required_probe_failure
        && receipts.iter().any(|receipt| {
            plan.probe_by_id(&receipt.identity.probe_id)
                .is_some_and(|probe| {
                    probe.spec().required
                        && !matches!(
                            receipt.status,
                            ProbeReceiptStatus::Succeeded | ProbeReceiptStatus::CacheHit
                        )
                })
        });
    required_failed
        || match rule.kind {
            ProbeCompletionRuleKind::AllTerminal => receipts.len() == plan.probes().len(),
            ProbeCompletionRuleKind::MinimumSuccesses => {
                completion_threshold_satisfied(plan, receipts)
                    && plan
                        .probes()
                        .iter()
                        .filter(|probe| probe.spec().required)
                        .all(|required| {
                            receipts.iter().any(|receipt| {
                                receipt.identity.probe_id == required.spec().probe_id
                                    && matches!(
                                        receipt.status,
                                        ProbeReceiptStatus::Succeeded
                                            | ProbeReceiptStatus::CacheHit
                                    )
                            })
                        })
            }
            ProbeCompletionRuleKind::FirstConclusive => false,
        }
}

fn completion_threshold_satisfied(
    plan: &ValidatedDebugProbePlan,
    receipts: &[ProbeExecutionReceipt],
) -> bool {
    if plan.plan().completion_rule.kind == ProbeCompletionRuleKind::AllTerminal {
        return true;
    }
    let completed = i64::try_from(
        receipts
            .iter()
            .filter(|receipt| {
                matches!(
                    receipt.status,
                    ProbeReceiptStatus::Succeeded
                        | ProbeReceiptStatus::Failed
                        | ProbeReceiptStatus::TimedOut
                        | ProbeReceiptStatus::CacheHit
                )
            })
            .count(),
    )
    .unwrap_or(i64::MAX);
    let successful = i64::try_from(
        receipts
            .iter()
            .filter(|receipt| {
                matches!(
                    receipt.status,
                    ProbeReceiptStatus::Succeeded | ProbeReceiptStatus::CacheHit
                )
            })
            .count(),
    )
    .unwrap_or(i64::MAX);
    let rule = &plan.plan().completion_rule;
    completed >= rule.minimum_completed_probes && successful >= rule.minimum_successful_probes
}

fn process_receipt(
    intent: &ProbeExecutionIntent,
    facts: ProbeRunFacts,
    manifest: &ProbeRawArtifactManifest,
    started_at: &Instant,
    finished_at: &Instant,
) -> ProbeExecutionReceipt {
    let ProbeRunFacts {
        termination,
        exit_code,
        signal,
        duration,
    } = facts;
    let duration_millis = i64::try_from(duration.as_millis()).unwrap_or(i64::MAX);
    let (status, error, exit_code, signal) = match termination {
        ProbeRunTermination::Exited if exit_code == Some(0) => {
            (ProbeReceiptStatus::Succeeded, None, Some(0), None)
        }
        ProbeRunTermination::Exited => (
            ProbeReceiptStatus::Failed,
            Some(probe_error(
                DebugProbeErrorCode::InfrastructureError,
                "probe exited non-zero",
            )),
            exit_code.filter(|code| *code != 0),
            signal,
        ),
        ProbeRunTermination::TimedOut => (
            ProbeReceiptStatus::TimedOut,
            Some(probe_error(
                DebugProbeErrorCode::TimedOut,
                "probe timed out",
            )),
            None,
            signal,
        ),
        ProbeRunTermination::Cancelled => (
            ProbeReceiptStatus::Cancelled,
            Some(probe_error(
                DebugProbeErrorCode::Cancelled,
                "probe was cancelled",
            )),
            None,
            signal,
        ),
        ProbeRunTermination::CleanupFailed => (
            ProbeReceiptStatus::Failed,
            Some(probe_error(
                DebugProbeErrorCode::ProcessCleanupFailed,
                "probe process cleanup failed",
            )),
            exit_code.filter(|code| *code != 0),
            signal,
        ),
        ProbeRunTermination::OutputLimitExceeded | ProbeRunTermination::InfrastructureError => (
            ProbeReceiptStatus::Failed,
            Some(probe_error(
                DebugProbeErrorCode::InfrastructureError,
                "probe execution infrastructure failed",
            )),
            exit_code.filter(|code| *code != 0),
            signal,
        ),
    };
    ProbeExecutionReceipt {
        artifact_refs: manifest.artifact_references(),
        duration_millis,
        error,
        exit_code,
        finished_at: finished_at.clone(),
        identity: intent.identity.clone(),
        output_bytes: manifest.output_bytes(),
        output_truncated: manifest.output_truncated(),
        plan_digest: intent.plan_digest.clone(),
        schema_version: 1,
        signal,
        started_at: started_at.clone(),
        status,
        timed_out: termination == ProbeRunTermination::TimedOut,
    }
}

fn apply_completion_cancellation(
    receipt: &mut ProbeExecutionReceipt,
    cancellation: &ProbeRunCancellation,
) {
    let cleanup_failed = receipt
        .error
        .as_ref()
        .is_some_and(|error| error.code == DebugProbeErrorCode::ProcessCleanupFailed);
    if cancellation.is_cancelled() && !cleanup_failed {
        receipt.status = ProbeReceiptStatus::Cancelled;
        receipt.error = Some(probe_error(
            DebugProbeErrorCode::Cancelled,
            "probe was cancelled",
        ));
        receipt.exit_code = None;
        receipt.signal = None;
        receipt.timed_out = false;
    }
}

fn canonical_round_receipt(
    plan: &ValidatedDebugProbePlan,
    receipts: Vec<ProbeExecutionReceipt>,
    recovery: &RoundRecoveryContext,
) -> Result<ProbeRoundReceipt, ProbeSchedulerError> {
    if receipts.len() != plan.probes().len() {
        return Err(journal_error());
    }
    let finished_at = receipts
        .iter()
        .map(|receipt| receipt.finished_at.clone())
        .max_by(|left, right| left.0.cmp(&right.0))
        .ok_or_else(journal_error)?;
    let usage = canonical_round_usage(plan, &receipts, recovery)?;
    let outcome = canonical_round_outcome(plan, &receipts, recovery);
    let receipt = ProbeRoundReceipt {
        authority: plan.plan().authority.clone(),
        completion_reason: outcome.completion_reason,
        error: outcome.error,
        finished_at,
        plan_digest: plan.plan().plan_digest.clone(),
        probe_receipts: receipts,
        schema_version: 1,
        started_at: recovery.started_at.clone(),
        status: outcome.status,
        usage,
    };
    validate_probe_round_receipt(&receipt, plan)
        .map_err(|error| ProbeSchedulerError::new(error.code().clone(), error.message()))?;
    Ok(receipt)
}

fn canonical_round_usage(
    plan: &ValidatedDebugProbePlan,
    receipts: &[ProbeExecutionReceipt],
    recovery: &RoundRecoveryContext,
) -> Result<ProbeRoundBudgetUsage, ProbeSchedulerError> {
    let executed = receipts
        .iter()
        .map(|receipt| !receipt.artifact_refs.is_empty())
        .collect::<Vec<_>>();
    let waves = recovered_plan_waves(plan)?;
    let mut peak_parallel_probes = 0_i64;
    let mut peak_memory_bytes = 0_i64;
    for wave in &waves {
        let active = wave
            .iter()
            .filter(|index| executed[**index])
            .collect::<Vec<_>>();
        peak_parallel_probes =
            peak_parallel_probes.max(i64::try_from(active.len()).unwrap_or(i64::MAX));
        peak_memory_bytes = peak_memory_bytes.max(
            active
                .iter()
                .map(|index| plan.probes()[**index].spec().resources.memory_limit_bytes)
                .sum(),
        );
    }
    let elapsed_millis = recovery
        .stop
        .map_or(recovery.observed_elapsed_millis, |stop| {
            stop.elapsed_millis.max(recovery.observed_elapsed_millis)
        })
        .min(plan.plan().budget.wall_time_limit_millis);
    let total_output_bytes = receipts.iter().map(|receipt| receipt.output_bytes).sum();
    let total_cpu_millis = plan
        .probes()
        .iter()
        .zip(&executed)
        .filter(|(_, executed)| **executed)
        .map(|(probe, _)| probe.spec().resources.cpu_limit_millis)
        .sum();
    let total_command_arg_bytes = plan
        .probes()
        .iter()
        .zip(&executed)
        .filter(|(_, executed)| **executed)
        .map(|(probe, _)| probe.spec().command.command_arg_bytes)
        .sum();
    Ok(ProbeRoundBudgetUsage {
        budget_digest: plan.plan().budget.budget_digest.clone(),
        elapsed_millis,
        peak_memory_bytes,
        peak_parallel_probes,
        probe_count: i64::try_from(plan.probes().len()).unwrap_or(i64::MAX),
        total_command_arg_bytes,
        total_cpu_millis,
        total_output_bytes,
    })
}

struct CanonicalRoundOutcome {
    status: ProbeRoundReceiptStatus,
    completion_reason: ProbeRoundCompletionReason,
    error: Option<DebugProbeError>,
}

fn canonical_round_outcome(
    plan: &ValidatedDebugProbePlan,
    receipts: &[ProbeExecutionReceipt],
    recovery: &RoundRecoveryContext,
) -> CanonicalRoundOutcome {
    let required_failed = plan.probes().iter().zip(receipts).any(|(probe, receipt)| {
        probe.spec().required
            && !matches!(
                receipt.status,
                ProbeReceiptStatus::Succeeded | ProbeReceiptStatus::CacheHit
            )
    });
    let budget_exhausted = recovery
        .stop
        .is_some_and(|stop| stop.reason == RoundStopReason::BudgetExhausted);
    let cancelled = recovery
        .stop
        .is_some_and(|stop| stop.reason == RoundStopReason::Cancelled)
        || receipts
            .iter()
            .any(|receipt| receipt.status == ProbeReceiptStatus::Cancelled);
    let completion_satisfied = completion_threshold_satisfied(plan, receipts);
    let status = if budget_exhausted {
        ProbeRoundReceiptStatus::Failed
    } else if cancelled {
        ProbeRoundReceiptStatus::Cancelled
    } else if required_failed || !completion_satisfied {
        ProbeRoundReceiptStatus::Failed
    } else {
        ProbeRoundReceiptStatus::Completed
    };
    let completion_reason = if budget_exhausted {
        ProbeRoundCompletionReason::BudgetExhausted
    } else if cancelled {
        ProbeRoundCompletionReason::Cancelled
    } else if status == ProbeRoundReceiptStatus::Completed && completion_reached(plan, receipts) {
        ProbeRoundCompletionReason::CompletionRuleSatisfied
    } else {
        ProbeRoundCompletionReason::AllProbesTerminal
    };
    let error = if budget_exhausted {
        Some(probe_error(
            DebugProbeErrorCode::BudgetExceeded,
            "probe round wall-time budget exhausted",
        ))
    } else if cancelled {
        Some(probe_error(
            DebugProbeErrorCode::Cancelled,
            "probe round cancelled",
        ))
    } else if required_failed {
        Some(probe_error(
            DebugProbeErrorCode::InfrastructureError,
            "required probe did not succeed",
        ))
    } else if !completion_satisfied {
        Some(probe_error(
            DebugProbeErrorCode::InfrastructureError,
            "probe completion threshold was not satisfied",
        ))
    } else {
        None
    };
    CanonicalRoundOutcome {
        status,
        completion_reason,
        error,
    }
}

fn recovered_plan_waves(
    plan: &ValidatedDebugProbePlan,
) -> Result<Vec<Vec<usize>>, ProbeSchedulerError> {
    let parallel_limit = usize::try_from(plan.plan().budget.parallel_probe_limit)
        .map_err(|_| budget_error("parallel probe limit is invalid"))?;
    let memory_limit = plan.plan().budget.peak_memory_limit_bytes;
    let mut waves: Vec<Vec<usize>> = Vec::new();
    for (index, probe) in plan.probes().iter().enumerate() {
        let resources = &probe.spec().resources;
        let selected = waves.iter().position(|wave| {
            wave.len() < parallel_limit
                && wave
                    .iter()
                    .map(|other| plan.probes()[*other].spec().resources.memory_limit_bytes)
                    .sum::<i64>()
                    .saturating_add(resources.memory_limit_bytes)
                    <= memory_limit
                && wave.iter().all(|other| {
                    !claims_conflict(&plan.probes()[*other].spec().resources, resources)
                })
        });
        if let Some(wave) = selected {
            waves[wave].push(index);
        } else {
            waves.push(vec![index]);
        }
    }
    Ok(waves)
}

fn skipped_receipt(
    intent: &ProbeExecutionIntent,
    now: &Instant,
    cancelled: bool,
) -> ProbeExecutionReceipt {
    let (status, error) = if cancelled {
        (
            ProbeReceiptStatus::Cancelled,
            Some(probe_error(
                DebugProbeErrorCode::Cancelled,
                "probe skipped after round cancellation",
            )),
        )
    } else {
        (ProbeReceiptStatus::Skipped, None)
    };
    ProbeExecutionReceipt {
        artifact_refs: Vec::new(),
        duration_millis: 0,
        error,
        exit_code: None,
        finished_at: now.clone(),
        identity: intent.identity.clone(),
        output_bytes: 0,
        output_truncated: false,
        plan_digest: intent.plan_digest.clone(),
        schema_version: 1,
        signal: None,
        started_at: intent.created_at.clone(),
        status,
        timed_out: false,
    }
}

fn probe_error(code: DebugProbeErrorCode, message: &str) -> DebugProbeError {
    DebugProbeError {
        code,
        message: message.to_owned(),
        retryable: false,
    }
}

fn canonicalize_claim_lists(claim: &mut ProbeResourceClaim) -> Result<(), ProbeSchedulerError> {
    sort_unique(&mut claim.paths)?;
    sort_unique(&mut claim.port_numbers)?;
    sort_unique(&mut claim.service_keys)?;
    sort_unique(&mut claim.database_keys)?;
    sort_unique(&mut claim.exclusive_keys)?;
    Ok(())
}

fn sort_unique<T: Ord>(values: &mut [T]) -> Result<(), ProbeSchedulerError> {
    values.sort();
    if values.windows(2).any(|pair| pair[0] == pair[1]) {
        return Err(invalid_probe("host resource list contains duplicates"));
    }
    Ok(())
}

fn validate_claim_paths(
    workspace_root: &Path,
    claim: &ProbeResourceClaim,
) -> Result<(), ProbeSchedulerError> {
    for path in &claim.paths {
        validate_lexical_path(path)?;
        let joined = workspace_root.join(path);
        if joined.exists() {
            let resolved = fs::canonicalize(&joined)
                .map_err(|_| invalid_probe("probe resource path cannot be resolved"))?;
            if !resolved.starts_with(workspace_root) {
                return Err(ProbeSchedulerError::new(
                    DebugProbeErrorCode::WorkspaceWriteForbidden,
                    "probe resource path leaves the checkout",
                ));
            }
        }
    }
    Ok(())
}

fn validate_lexical_path(path: &str) -> Result<(), ProbeSchedulerError> {
    if path.is_empty()
        || path.contains("//")
        || Path::new(path).is_absolute()
        || Path::new(path)
            .components()
            .any(|component| matches!(component, Component::ParentDir | Component::RootDir))
    {
        return Err(ProbeSchedulerError::new(
            DebugProbeErrorCode::WorkspaceWriteForbidden,
            "probe path is outside the relative workspace boundary",
        ));
    }
    Ok(())
}

fn canonical_workspace(path: &Path) -> Result<PathBuf, ProbeSchedulerError> {
    let path =
        fs::canonicalize(path).map_err(|_| invalid_probe("probe workspace cannot be opened"))?;
    if !path.is_dir() {
        return Err(invalid_probe("probe workspace is not a directory"));
    }
    workspace_path(&path)?;
    Ok(path)
}

fn replay_workspace(path: &Path) -> Result<PathBuf, ProbeSchedulerError> {
    match fs::canonicalize(path) {
        Ok(canonical) if canonical.is_dir() => {
            workspace_path(&canonical)?;
            Ok(canonical)
        }
        Ok(_) => Err(invalid_probe("probe workspace is not a directory")),
        Err(_) => absolute_workspace_request(path),
    }
}

fn absolute_workspace_request(path: &Path) -> Result<PathBuf, ProbeSchedulerError> {
    let absolute = if path.is_absolute() {
        path.to_path_buf()
    } else {
        std::env::current_dir()
            .map_err(|_| invalid_probe("probe workspace cannot be opened"))?
            .join(path)
    };
    let mut normalized = PathBuf::new();
    for component in absolute.components() {
        match component {
            Component::Prefix(prefix) => normalized.push(prefix.as_os_str()),
            Component::RootDir => normalized.push(Path::new("/")),
            Component::CurDir => {}
            Component::ParentDir => {
                if !normalized.pop() {
                    return Err(invalid_probe("probe workspace path is not canonical"));
                }
            }
            Component::Normal(part) => normalized.push(part),
        }
    }
    workspace_path(&normalized)?;
    Ok(normalized)
}

fn workspace_path(path: &Path) -> Result<&str, ProbeSchedulerError> {
    path.to_str()
        .ok_or_else(|| invalid_probe("probe workspace path is not canonical UTF-8"))
}

fn monotonic_elapsed_millis(started_at: MonotonicInstant) -> i64 {
    i64::try_from(started_at.elapsed().as_millis()).unwrap_or(i64::MAX)
}

fn resolve_existing_path(
    workspace_root: &Path,
    relative: &str,
    message: &'static str,
) -> Result<PathBuf, ProbeSchedulerError> {
    validate_lexical_path(relative)?;
    let resolved = fs::canonicalize(workspace_root.join(relative))
        .map_err(|_| invalid_probe("probe working directory cannot be resolved"))?;
    if !resolved.starts_with(workspace_root) {
        return Err(ProbeSchedulerError::new(
            DebugProbeErrorCode::WorkspaceWriteForbidden,
            message,
        ));
    }
    Ok(resolved)
}

async fn join_all<F>(futures: Vec<F>) -> Vec<F::Output>
where
    F: Future + Unpin,
{
    let mut futures = futures.into_iter().map(Some).collect::<Vec<_>>();
    let mut outputs = (0..futures.len()).map(|_| None).collect::<Vec<_>>();
    std::future::poll_fn(|context| {
        let mut pending = false;
        for (index, slot) in futures.iter_mut().enumerate() {
            let Some(future) = slot.as_mut() else {
                continue;
            };
            match Pin::new(future).poll(context) {
                Poll::Ready(output) => {
                    outputs[index] = Some(output);
                    *slot = None;
                }
                Poll::Pending => pending = true,
            }
        }
        if pending {
            Poll::Pending
        } else {
            Poll::Ready(
                outputs
                    .iter_mut()
                    .map(|output| output.take().expect("completed probe future has output"))
                    .collect(),
            )
        }
    })
    .await
}

enum RoundClaim {
    Claimed,
    Terminal(Box<ProbeRoundReceipt>),
    Unresolved,
}

enum RoundReplay {
    Missing,
    Terminal(Box<ProbeRoundReceipt>),
    Unresolved,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum RoundStopReason {
    Cancelled,
    BudgetExhausted,
}

impl RoundStopReason {
    const fn as_str(self) -> &'static str {
        match self {
            Self::Cancelled => "cancelled",
            Self::BudgetExhausted => "budget_exhausted",
        }
    }

    fn parse(value: &str) -> Result<Self, ProbeSchedulerError> {
        match value {
            "cancelled" => Ok(Self::Cancelled),
            "budget_exhausted" => Ok(Self::BudgetExhausted),
            _ => Err(journal_error()),
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct RetainedRoundStop {
    reason: RoundStopReason,
    elapsed_millis: i64,
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct RoundRecoveryContext {
    workspace_root: PathBuf,
    started_at: Instant,
    observed_elapsed_millis: i64,
    stop: Option<RetainedRoundStop>,
}

struct ProbeJournal {
    connection: Connection,
}

impl ProbeJournal {
    fn open(root: impl AsRef<Path>) -> Result<Self, ProbeSchedulerError> {
        let directory = root.as_ref().join(DATABASE_DIRECTORY);
        ensure_private_directory(&directory)?;
        let database = directory.join(DATABASE_FILE);
        ensure_private_file(&database)?;
        let connection = Connection::open(&database).map_err(|_| journal_error())?;
        connection
            .execute_batch(
                "PRAGMA journal_mode=WAL;
                 PRAGMA synchronous=FULL;
                 PRAGMA foreign_keys=ON;",
            )
            .map_err(|_| journal_error())?;
        connection
            .execute_batch(SCHEMA)
            .map_err(|_| journal_error())?;
        Ok(Self { connection })
    }

    fn claim_round(
        &mut self,
        plan: &ValidatedDebugProbePlan,
        admitted: &[AdmittedProbe],
        workspace_root: &Path,
        workspace_request_root: &Path,
        now: &Instant,
    ) -> Result<RoundClaim, ProbeSchedulerError> {
        let round_id = &plan.plan().authority.round_id.0;
        match self.replay_round(plan, workspace_root, workspace_request_root, false)? {
            RoundReplay::Terminal(receipt) => return Ok(RoundClaim::Terminal(receipt)),
            RoundReplay::Unresolved => return Ok(RoundClaim::Unresolved),
            RoundReplay::Missing => {}
        }

        let plan_bytes = serde_json::to_vec(plan.plan()).map_err(|_| journal_error())?;
        let transaction = self.connection.transaction().map_err(|_| journal_error())?;
        transaction
            .execute(
                "INSERT INTO probe_round
                   (round_id, plan_json, receipt_json, workspace_root, workspace_request_root,
                    observed_elapsed_millis,
                    stop_reason, stop_elapsed_millis, created_at, updated_at)
                 VALUES (?1, ?2, NULL, ?3, ?4, 0, NULL, NULL, ?5, ?5)",
                params![
                    round_id,
                    plan_bytes,
                    workspace_path(workspace_root)?,
                    workspace_path(workspace_request_root)?,
                    now.0
                ],
            )
            .map_err(|_| journal_error())?;
        for probe in admitted {
            let intent = probe.intent.intent();
            let ordinal = plan
                .probe_by_id(&intent.identity.probe_id)
                .map(|probe| i64::try_from(probe.ordinal()).unwrap_or(i64::MAX))
                .ok_or_else(journal_error)?;
            let bytes = serde_json::to_vec(intent).map_err(|_| journal_error())?;
            transaction
                .execute(
                    "INSERT INTO probe_execution
                       (probe_execution_id, round_id, ordinal, intent_json, receipt_json,
                        created_at, updated_at)
                     VALUES (?1, ?2, ?3, ?4, NULL, ?5, ?5)",
                    params![
                        intent.identity.probe_execution_id.0,
                        round_id,
                        ordinal,
                        bytes,
                        now.0
                    ],
                )
                .map_err(|_| journal_error())?;
        }
        transaction.commit().map_err(|_| journal_error())?;
        Ok(RoundClaim::Claimed)
    }

    fn replay_round(
        &self,
        plan: &ValidatedDebugProbePlan,
        workspace_root: &Path,
        workspace_request_root: &Path,
        workspace_missing: bool,
    ) -> Result<RoundReplay, ProbeSchedulerError> {
        let round_id = &plan.plan().authority.round_id.0;
        let plan_bytes = serde_json::to_vec(plan.plan()).map_err(|_| journal_error())?;
        let existing = self
            .connection
            .query_row(
                "SELECT plan_json, receipt_json, workspace_root, workspace_request_root
                 FROM probe_round WHERE round_id = ?1",
                [round_id],
                |row| {
                    Ok((
                        row.get::<_, Vec<u8>>(0)?,
                        row.get::<_, Option<Vec<u8>>>(1)?,
                        row.get::<_, String>(2)?,
                        row.get::<_, String>(3)?,
                    ))
                },
            )
            .optional()
            .map_err(|_| journal_error())?;
        let Some((stored_plan, stored_receipt, stored_workspace_root, stored_request_root)) =
            existing
        else {
            return Ok(RoundReplay::Missing);
        };
        if stored_plan != plan_bytes {
            return Err(stale_authority("probe round replay changed its plan bytes"));
        }
        let workspace_matches = if workspace_missing {
            stored_request_root == workspace_path(workspace_request_root)?
        } else {
            stored_workspace_root == workspace_path(workspace_root)?
        };
        if !workspace_matches {
            return Err(stale_authority(
                "probe round replay changed its canonical workspace root",
            ));
        }
        let Some(bytes) = stored_receipt else {
            return Ok(RoundReplay::Unresolved);
        };
        let receipt =
            serde_json::from_slice::<ProbeRoundReceipt>(&bytes).map_err(|_| journal_error())?;
        validate_probe_round_receipt(&receipt, plan).map_err(|_| journal_error())?;
        self.validate_terminal_execution_rows(round_id, &receipt)?;
        Ok(RoundReplay::Terminal(Box::new(receipt)))
    }

    fn load_intents(
        &self,
        plan: &ValidatedDebugProbePlan,
    ) -> Result<Vec<ProbeExecutionIntent>, ProbeSchedulerError> {
        let mut statement = self
            .connection
            .prepare(
                "SELECT intent_json FROM probe_execution
                 WHERE round_id = ?1 ORDER BY ordinal ASC",
            )
            .map_err(|_| journal_error())?;
        let rows = statement
            .query_map([&plan.plan().authority.round_id.0], |row| {
                row.get::<_, Vec<u8>>(0)
            })
            .map_err(|_| journal_error())?;
        let mut intents = Vec::new();
        for row in rows {
            let bytes = row.map_err(|_| journal_error())?;
            let intent = serde_json::from_slice::<ProbeExecutionIntent>(&bytes)
                .map_err(|_| journal_error())?;
            if serde_json::to_vec(&intent).map_err(|_| journal_error())? != bytes {
                return Err(journal_error());
            }
            seal_probe_execution_intent(intent.clone(), plan).map_err(|_| journal_error())?;
            intents.push(intent);
        }
        if intents.len() != plan.probes().len() {
            return Err(journal_error());
        }
        Ok(intents)
    }

    fn recovery_context(
        &self,
        plan: &ValidatedDebugProbePlan,
    ) -> Result<RoundRecoveryContext, ProbeSchedulerError> {
        let value = self
            .connection
            .query_row(
                "SELECT workspace_root, created_at, observed_elapsed_millis,
                        stop_reason, stop_elapsed_millis
                 FROM probe_round WHERE round_id = ?1",
                [&plan.plan().authority.round_id.0],
                |row| {
                    Ok((
                        row.get::<_, String>(0)?,
                        row.get::<_, String>(1)?,
                        row.get::<_, i64>(2)?,
                        row.get::<_, Option<String>>(3)?,
                        row.get::<_, Option<i64>>(4)?,
                    ))
                },
            )
            .optional()
            .map_err(|_| journal_error())?
            .ok_or_else(journal_error)?;
        let (workspace_root, started_at, observed_elapsed, stop_reason, stop_elapsed) = value;
        if observed_elapsed < 0 || stop_reason.is_some() != stop_elapsed.is_some() {
            return Err(journal_error());
        }
        let stop = stop_reason
            .zip(stop_elapsed)
            .map(|(reason, elapsed_millis)| {
                if elapsed_millis < 0 {
                    return Err(journal_error());
                }
                Ok(RetainedRoundStop {
                    reason: RoundStopReason::parse(&reason)?,
                    elapsed_millis,
                })
            })
            .transpose()?;
        Ok(RoundRecoveryContext {
            workspace_root: PathBuf::from(workspace_root),
            started_at: Instant(started_at),
            observed_elapsed_millis: observed_elapsed,
            stop,
        })
    }

    fn observe_round_elapsed(
        &mut self,
        round_id: &str,
        elapsed_millis: i64,
        now: &Instant,
    ) -> Result<(), ProbeSchedulerError> {
        if elapsed_millis < 0 {
            return Err(journal_error());
        }
        let changed = self
            .connection
            .execute(
                "UPDATE probe_round
                 SET updated_at = CASE
                         WHEN observed_elapsed_millis < ?2 THEN ?3 ELSE updated_at END,
                     observed_elapsed_millis = MAX(observed_elapsed_millis, ?2)
                 WHERE round_id = ?1 AND receipt_json IS NULL",
                params![round_id, elapsed_millis, now.0],
            )
            .map_err(|_| journal_error())?;
        if changed == 1 {
            Ok(())
        } else {
            Err(journal_error())
        }
    }

    fn retain_round_stop(
        &mut self,
        round_id: &str,
        reason: RoundStopReason,
        elapsed_millis: i64,
        now: &Instant,
    ) -> Result<RetainedRoundStop, ProbeSchedulerError> {
        if elapsed_millis < 0 {
            return Err(journal_error());
        }
        self.connection
            .execute(
                "UPDATE probe_round
                 SET stop_reason = ?2, stop_elapsed_millis = ?3, updated_at = ?4
                 WHERE round_id = ?1 AND receipt_json IS NULL AND stop_reason IS NULL",
                params![round_id, reason.as_str(), elapsed_millis, now.0],
            )
            .map_err(|_| journal_error())?;
        let stored = self
            .connection
            .query_row(
                "SELECT stop_reason, stop_elapsed_millis
                 FROM probe_round WHERE round_id = ?1 AND receipt_json IS NULL",
                [round_id],
                |row| Ok((row.get::<_, String>(0)?, row.get::<_, i64>(1)?)),
            )
            .optional()
            .map_err(|_| journal_error())?
            .ok_or_else(journal_error)?;
        Ok(RetainedRoundStop {
            reason: RoundStopReason::parse(&stored.0)?,
            elapsed_millis: stored.1,
        })
    }

    fn load_probe_receipt(
        &self,
        intent: &ProbeExecutionIntent,
    ) -> Result<Option<ProbeExecutionReceipt>, ProbeSchedulerError> {
        let bytes = self
            .connection
            .query_row(
                "SELECT receipt_json FROM probe_execution WHERE probe_execution_id = ?1",
                [&intent.identity.probe_execution_id.0],
                |row| row.get::<_, Option<Vec<u8>>>(0),
            )
            .optional()
            .map_err(|_| journal_error())?
            .ok_or_else(journal_error)?;
        bytes
            .map(|bytes| {
                let receipt = serde_json::from_slice::<ProbeExecutionReceipt>(&bytes)
                    .map_err(|_| journal_error())?;
                if serde_json::to_vec(&receipt).map_err(|_| journal_error())? != bytes
                    || receipt.identity != intent.identity
                    || receipt.plan_digest != intent.plan_digest
                {
                    return Err(journal_error());
                }
                Ok(receipt)
            })
            .transpose()
    }

    fn validate_terminal_execution_rows(
        &self,
        round_id: &str,
        receipt: &ProbeRoundReceipt,
    ) -> Result<(), ProbeSchedulerError> {
        let mut statement = self
            .connection
            .prepare(
                "SELECT receipt_json FROM probe_execution
                 WHERE round_id = ?1 ORDER BY ordinal ASC",
            )
            .map_err(|_| journal_error())?;
        let rows = statement
            .query_map([round_id], |row| row.get::<_, Option<Vec<u8>>>(0))
            .map_err(|_| journal_error())?;
        let mut retained = Vec::new();
        for row in rows {
            let bytes = row
                .map_err(|_| journal_error())?
                .ok_or_else(journal_error)?;
            retained.push(
                serde_json::from_slice::<ProbeExecutionReceipt>(&bytes)
                    .map_err(|_| journal_error())?,
            );
        }
        if retained == receipt.probe_receipts {
            Ok(())
        } else {
            Err(journal_error())
        }
    }

    fn retain_probe_receipt(
        &mut self,
        receipt: &ProbeExecutionReceipt,
        now: &Instant,
    ) -> Result<(), ProbeSchedulerError> {
        let id = &receipt.identity.probe_execution_id.0;
        let bytes = serde_json::to_vec(receipt).map_err(|_| journal_error())?;
        let existing = self
            .connection
            .query_row(
                "SELECT receipt_json FROM probe_execution WHERE probe_execution_id = ?1",
                [id],
                |row| row.get::<_, Option<Vec<u8>>>(0),
            )
            .optional()
            .map_err(|_| journal_error())?
            .flatten();
        if let Some(existing) = existing {
            return if existing == bytes {
                Ok(())
            } else {
                Err(journal_error())
            };
        }
        let changed = self
            .connection
            .execute(
                "UPDATE probe_execution SET receipt_json = ?2, updated_at = ?3
                 WHERE probe_execution_id = ?1 AND receipt_json IS NULL",
                params![id, bytes, now.0],
            )
            .map_err(|_| journal_error())?;
        if changed == 1 {
            Ok(())
        } else {
            Err(journal_error())
        }
    }

    fn retain_round_receipt(
        &mut self,
        receipt: &ProbeRoundReceipt,
        now: &Instant,
    ) -> Result<(), ProbeSchedulerError> {
        let id = &receipt.authority.round_id.0;
        let bytes = serde_json::to_vec(receipt).map_err(|_| journal_error())?;
        let existing = self
            .connection
            .query_row(
                "SELECT receipt_json FROM probe_round WHERE round_id = ?1",
                [id],
                |row| row.get::<_, Option<Vec<u8>>>(0),
            )
            .optional()
            .map_err(|_| journal_error())?
            .flatten();
        if let Some(existing) = existing {
            return if existing == bytes {
                Ok(())
            } else {
                Err(journal_error())
            };
        }
        let transaction = self.connection.transaction().map_err(|_| journal_error())?;
        let incomplete = transaction
            .query_row(
                "SELECT COUNT(*) FROM probe_execution
                 WHERE round_id = ?1 AND receipt_json IS NULL",
                [id],
                |row| row.get::<_, i64>(0),
            )
            .map_err(|_| journal_error())?;
        if incomplete != 0 {
            return Err(journal_error());
        }
        let changed = transaction
            .execute(
                "UPDATE probe_round SET receipt_json = ?2, updated_at = ?3
                 WHERE round_id = ?1 AND receipt_json IS NULL",
                params![id, bytes, now.0],
            )
            .map_err(|_| journal_error())?;
        if changed != 1 {
            return Err(journal_error());
        }
        transaction.commit().map_err(|_| journal_error())
    }
}

#[cfg(unix)]
fn ensure_private_directory(path: &Path) -> Result<(), ProbeSchedulerError> {
    use std::os::unix::fs::{DirBuilderExt as _, PermissionsExt as _};

    match fs::symlink_metadata(path) {
        Ok(metadata) if metadata.file_type().is_dir() => {
            let mode = fs::metadata(path)
                .map_err(|_| journal_error())?
                .permissions()
                .mode();
            if mode.trailing_zeros() >= 6 {
                Ok(())
            } else {
                Err(journal_error())
            }
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => fs::DirBuilder::new()
            .recursive(false)
            .mode(0o700)
            .create(path)
            .map_err(|_| journal_error()),
        Ok(_) | Err(_) => Err(journal_error()),
    }
}

#[cfg(unix)]
fn ensure_private_file(path: &Path) -> Result<(), ProbeSchedulerError> {
    use std::os::unix::fs::{OpenOptionsExt as _, PermissionsExt as _};

    let file = match fs::symlink_metadata(path) {
        Ok(metadata) if metadata.file_type().is_file() => fs::OpenOptions::new()
            .read(true)
            .write(true)
            .open(path)
            .map_err(|_| journal_error())?,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => fs::OpenOptions::new()
            .read(true)
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(path)
            .map_err(|_| journal_error())?,
        Ok(_) | Err(_) => return Err(journal_error()),
    };
    if file
        .metadata()
        .map_err(|_| journal_error())?
        .permissions()
        .mode()
        .trailing_zeros()
        < 6
    {
        return Err(journal_error());
    }
    Ok(())
}

fn system_instant() -> Result<Instant, ProbeSchedulerError> {
    let duration = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_err(|_| journal_error())?;
    let seconds = i64::try_from(duration.as_secs()).map_err(|_| journal_error())?;
    let days = seconds.div_euclid(86_400);
    let second_of_day = seconds.rem_euclid(86_400);
    let (year, month, day) = civil_from_days(days);
    let hour = second_of_day / 3_600;
    let minute = second_of_day % 3_600 / 60;
    let second = second_of_day % 60;
    Ok(Instant(format!(
        "{year:04}-{month:02}-{day:02}T{hour:02}:{minute:02}:{second:02}.{:03}Z",
        duration.subsec_millis()
    )))
}

fn civil_from_days(days_since_unix_epoch: i64) -> (i64, i64, i64) {
    let days = days_since_unix_epoch + 719_468;
    let era = if days >= 0 { days } else { days - 146_096 } / 146_097;
    let day_of_era = days - era * 146_097;
    let year_of_era =
        (day_of_era - day_of_era / 1_460 + day_of_era / 36_524 - day_of_era / 146_096) / 365;
    let mut year = year_of_era + era * 400;
    let day_of_year = day_of_era - (365 * year_of_era + year_of_era / 4 - year_of_era / 100);
    let month_prime = (5 * day_of_year + 2) / 153;
    let day = day_of_year - (153 * month_prime + 2) / 5 + 1;
    let month = month_prime + if month_prime < 10 { 3 } else { -9 };
    year += i64::from(month <= 2);
    (year, month, day)
}

fn invalid_probe(message: &'static str) -> ProbeSchedulerError {
    ProbeSchedulerError::new(DebugProbeErrorCode::InvalidProbe, message)
}

fn stale_authority(message: &'static str) -> ProbeSchedulerError {
    ProbeSchedulerError::new(DebugProbeErrorCode::StaleAuthority, message)
}

fn resource_conflict(message: &'static str) -> ProbeSchedulerError {
    ProbeSchedulerError::new(DebugProbeErrorCode::ResourceConflict, message)
}

fn budget_error(message: &'static str) -> ProbeSchedulerError {
    ProbeSchedulerError::new(DebugProbeErrorCode::BudgetExceeded, message)
}

fn journal_error() -> ProbeSchedulerError {
    ProbeSchedulerError::new(
        DebugProbeErrorCode::InfrastructureError,
        "probe scheduler journal is unavailable",
    )
}

fn evidence_missing() -> ProbeSchedulerError {
    ProbeSchedulerError::new(
        DebugProbeErrorCode::InfrastructureError,
        "probe evidence is unavailable",
    )
}

fn evidence_store_error(error: &ProbeEvidenceStoreError) -> ProbeSchedulerError {
    let code = match error.kind() {
        ProbeEvidenceStoreErrorKind::InvalidInput => DebugProbeErrorCode::InvalidProbe,
        ProbeEvidenceStoreErrorKind::Conflict => DebugProbeErrorCode::StaleAuthority,
        ProbeEvidenceStoreErrorKind::DigestMismatch => DebugProbeErrorCode::ArtifactDigestMismatch,
        ProbeEvidenceStoreErrorKind::NotFound
        | ProbeEvidenceStoreErrorKind::Corrupt
        | ProbeEvidenceStoreErrorKind::Unavailable => DebugProbeErrorCode::InfrastructureError,
    };
    ProbeSchedulerError::new(code, "probe evidence durable state is unavailable")
}
