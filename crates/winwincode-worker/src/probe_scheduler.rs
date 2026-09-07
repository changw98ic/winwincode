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
        ValidatedDebugProbePlan, seal_debug_probe_plan, seal_probe_execution_intent,
        validate_probe_execution_receipt, validate_probe_round_receipt,
    },
    generated::{
        DebugProbeError, DebugProbeErrorCode, DebugProbeKind, DebugProbePlan,
        DebugProbeRoundAuthority, ProbeCommandSpec, ProbeCompletionRuleKind, ProbeExecutionIntent,
        ProbeExecutionReceipt, ProbeReceiptStatus, ProbeResourceClaim, ProbeRoundBudgetUsage,
        ProbeRoundCompletionReason, ProbeRoundReceipt, ProbeRoundReceiptStatus,
        ProbeSideEffectClass, ProbeWorkspaceAccess,
    },
};

const DATABASE_DIRECTORY: &str = ".probe-scheduler";
const DATABASE_FILE: &str = "probe-scheduler.sqlite3";
const SCHEMA: &str = "
CREATE TABLE IF NOT EXISTS probe_round (
    round_id TEXT PRIMARY KEY,
    plan_json BLOB NOT NULL,
    receipt_json BLOB,
    created_at TEXT NOT NULL,
    updated_at TEXT NOT NULL
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
        Ok(Self {
            kind,
            command,
            resources,
            max_timeout_millis,
            max_output_limit_bytes,
        })
    }

    fn admits(&self, spec: &winwincode_execution_port::generated::ProbeSpec) -> bool {
        self.kind == spec.kind
            && self.command == spec.command
            && spec.timeout_millis <= self.max_timeout_millis
            && spec.output_limit_bytes <= self.max_output_limit_bytes
    }
}

/// Host-admitted input given to the narrow process adapter.
#[derive(Clone, Debug)]
pub struct AdmittedProbe {
    intent: ProbeExecutionIntent,
    host_resources: ProbeResourceClaim,
    workspace_root: PathBuf,
    working_directory: PathBuf,
}

impl AdmittedProbe {
    /// Exact durable intent retained before this value reaches a runner.
    #[must_use]
    pub const fn intent(&self) -> &ProbeExecutionIntent {
        &self.intent
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

/// Process-level result consumed by the scheduler without exposing raw output
/// to the model feedback boundary.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ProbeRunResult {
    termination: ProbeRunTermination,
    exit_code: Option<i64>,
    signal: Option<String>,
    duration: Duration,
    output_bytes: usize,
    output_truncated: bool,
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
    /// Creates a bounded runner result.
    #[must_use]
    pub const fn new(
        termination: ProbeRunTermination,
        exit_code: Option<i64>,
        signal: Option<String>,
        duration: Duration,
        output_bytes: usize,
        output_truncated: bool,
    ) -> Self {
        Self {
            termination,
            exit_code,
            signal,
            duration,
            output_bytes,
            output_truncated,
        }
    }
}

/// Boxed runner future used by production and deterministic fixture adapters.
pub type ProbeRunnerFuture<'runner> =
    Pin<Box<dyn Future<Output = ProbeRunResult> + Send + 'runner>>;

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
}

/// Deep Worker-owned facade for one complete `DebugProbe` scheduling round.
pub struct ProbeScheduler<Runner, Clock = SystemProbeClock> {
    journal: Mutex<ProbeJournal>,
    runner: Arc<Runner>,
    templates: Vec<TrustedPureReadTemplate>,
    active: Mutex<BTreeMap<String, ActiveRound>>,
    clock: Clock,
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
        Ok(Self {
            journal: Mutex::new(ProbeJournal::open(root)?),
            runner: Arc::new(runner),
            templates,
            active: Mutex::new(BTreeMap::new()),
            clock,
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
        match self
            .journal
            .lock()
            .map_err(|_| journal_error())?
            .replay_round(&validated)?
        {
            RoundReplay::Terminal(receipt) => return Ok(*receipt),
            RoundReplay::Unresolved => {
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
                .claim_round(&validated, &admitted, &round_started_at)?;
            match claim {
                RoundClaim::Terminal(receipt) => return Ok(*receipt),
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
                &round_started_at,
                round_started_monotonic,
            )
            .await;
        if result.is_err() {
            self.remove_active(&round_key)?;
        }
        result
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
        round.cancellation.cancel();
        Ok(())
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
            admitted.push(AdmittedProbe {
                intent: intent.into_intent(),
                host_resources: template.resources.clone(),
                workspace_root: workspace_root.to_path_buf(),
                working_directory,
            });
        }
        admitted.sort_by(|left, right| {
            left.intent
                .identity
                .probe_id
                .0
                .cmp(&right.intent.identity.probe_id.0)
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
        round_started_at: &Instant,
        round_started_monotonic: MonotonicInstant,
    ) -> Result<ProbeRoundReceipt, ProbeSchedulerError> {
        let wall_limit = Duration::from_millis(
            u64::try_from(plan.plan().budget.wall_time_limit_millis)
                .map_err(|_| budget_error("round wall-time budget is invalid"))?,
        );
        let round_deadline = round_started_monotonic + wall_limit;
        let mut receipts = Vec::with_capacity(admitted.len());
        let mut executed = BTreeSet::new();
        let mut total_output_bytes = 0_i64;
        let mut total_cpu_millis = 0_i64;
        let mut total_command_arg_bytes = 0_i64;
        let mut peak_memory_bytes = 0_i64;
        let mut peak_parallel_probes = 0_i64;
        let mut cancelled = false;
        let mut rule_satisfied = false;

        for wave in waves {
            if MonotonicInstant::now() >= round_deadline {
                cancellation.exhaust_budget();
            }
            if cancellation.is_cancelled() {
                cancelled = true;
                break;
            }
            peak_parallel_probes =
                peak_parallel_probes.max(i64::try_from(wave.len()).unwrap_or(i64::MAX));
            peak_memory_bytes = peak_memory_bytes.max(
                wave.iter()
                    .map(|index| admitted[*index].host_resources.memory_limit_bytes)
                    .sum(),
            );
            let wave_started_at = self.clock.now()?;
            let futures = wave
                .iter()
                .map(|index| {
                    self.runner
                        .execute(admitted[*index].clone(), cancellation.clone())
                })
                .collect::<Vec<_>>();
            let mut joined = Box::pin(join_all(futures));
            let results = tokio::select! {
                results = &mut joined => results,
                () = tokio::time::sleep_until(tokio::time::Instant::from_std(round_deadline)) => {
                    cancellation.exhaust_budget();
                    joined.await
                }
            };
            let wave_finished_at = self.clock.now()?;
            for (index, result) in wave.into_iter().zip(results) {
                let mut receipt = process_receipt(
                    &admitted[index].intent,
                    result,
                    &wave_started_at,
                    &wave_finished_at,
                );
                if receipt.status == ProbeReceiptStatus::Cancelled {
                    cancellation.cancel();
                }
                if MonotonicInstant::now() >= round_deadline {
                    cancellation.exhaust_budget();
                }
                self.persist_probe_receipt_current(
                    plan,
                    &admitted[index].intent,
                    &mut receipt,
                    &cancellation,
                    &wave_finished_at,
                )?;
                total_cpu_millis = total_cpu_millis
                    .saturating_add(admitted[index].host_resources.cpu_limit_millis);
                total_command_arg_bytes = total_command_arg_bytes
                    .saturating_add(admitted[index].intent.spec.command.command_arg_bytes);
                total_output_bytes = total_output_bytes.saturating_add(receipt.output_bytes);
                cancelled |= receipt.status == ProbeReceiptStatus::Cancelled;
                executed.insert(index);
                receipts.push(receipt);
            }
            if completion_reached(plan, &receipts) {
                rule_satisfied = true;
                break;
            }
        }

        for (index, probe) in admitted.iter().enumerate() {
            if executed.contains(&index) {
                continue;
            }
            let now = self.clock.now()?;
            cancelled |= cancellation.is_cancelled();
            let mut receipt = skipped_receipt(&probe.intent, &now, cancelled);
            self.persist_probe_receipt_current(
                plan,
                &probe.intent,
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

        let required_failed = receipts.iter().any(|receipt| {
            admitted.iter().any(|probe| {
                probe.intent.identity.probe_id == receipt.identity.probe_id
                    && probe.intent.spec.required
                    && !matches!(
                        receipt.status,
                        ProbeReceiptStatus::Succeeded | ProbeReceiptStatus::CacheHit
                    )
            })
        });
        let completion_satisfied = completion_threshold_satisfied(plan, &receipts);
        if MonotonicInstant::now() >= round_deadline {
            cancellation.exhaust_budget();
        }
        let budget_exhausted = cancellation.is_budget_exhausted();
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
        } else if status == ProbeRoundReceiptStatus::Completed
            && rule_satisfied
            && completion_satisfied
        {
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
        let round_finished_at = self.clock.now()?;
        let actual_elapsed_millis =
            i64::try_from(round_started_monotonic.elapsed().as_millis()).unwrap_or(i64::MAX);
        let elapsed_millis = actual_elapsed_millis.min(plan.plan().budget.wall_time_limit_millis);
        let mut receipt = ProbeRoundReceipt {
            authority: plan.plan().authority.clone(),
            completion_reason,
            error,
            finished_at: round_finished_at.clone(),
            plan_digest: plan.plan().plan_digest.clone(),
            probe_receipts: receipts,
            schema_version: 1,
            started_at: round_started_at.clone(),
            status,
            usage: ProbeRoundBudgetUsage {
                budget_digest: plan.plan().budget.budget_digest.clone(),
                elapsed_millis,
                peak_memory_bytes,
                peak_parallel_probes,
                probe_count: i64::try_from(admitted.len()).unwrap_or(i64::MAX),
                total_command_arg_bytes,
                total_cpu_millis,
                total_output_bytes,
            },
        };
        self.persist_round_receipt_current(plan, &mut receipt, &cancellation, &round_finished_at)?;
        Ok(receipt)
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

    fn persist_round_receipt_current(
        &self,
        plan: &ValidatedDebugProbePlan,
        receipt: &mut ProbeRoundReceipt,
        cancellation: &ProbeRunCancellation,
        now: &Instant,
    ) -> Result<(), ProbeSchedulerError> {
        let mut active = self.active.lock().map_err(|_| journal_error())?;
        let Some(round) = active.get(&plan.plan().authority.round_id.0) else {
            return Err(stale_authority("probe round ended under stale authority"));
        };
        if round.authority != plan.plan().authority
            || !Arc::ptr_eq(&round.cancellation.0, &cancellation.0)
        {
            return Err(stale_authority("probe round authority was replaced"));
        }
        if round.cancellation.is_budget_exhausted() {
            receipt.status = ProbeRoundReceiptStatus::Failed;
            receipt.completion_reason = ProbeRoundCompletionReason::BudgetExhausted;
            receipt.error = Some(probe_error(
                DebugProbeErrorCode::BudgetExceeded,
                "probe round wall-time budget exhausted",
            ));
        } else if round.cancellation.is_cancelled() {
            receipt.status = ProbeRoundReceiptStatus::Cancelled;
            receipt.completion_reason = ProbeRoundCompletionReason::Cancelled;
            receipt.error = Some(probe_error(
                DebugProbeErrorCode::Cancelled,
                "probe round cancelled",
            ));
        }
        validate_probe_round_receipt(receipt, plan)
            .map_err(|error| ProbeSchedulerError::new(error.code().clone(), error.message()))?;
        self.journal
            .lock()
            .map_err(|_| journal_error())?
            .retain_round_receipt(receipt, now)?;
        active.remove(&plan.plan().authority.round_id.0);
        Ok(())
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
            .map(|index| admitted[*index].intent.spec.timeout_millis)
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
    result: ProbeRunResult,
    started_at: &Instant,
    finished_at: &Instant,
) -> ProbeExecutionReceipt {
    let duration_millis = i64::try_from(result.duration.as_millis()).unwrap_or(i64::MAX);
    let output_bytes = i64::try_from(result.output_bytes).unwrap_or(i64::MAX);
    let (status, error, exit_code, signal) = match result.termination {
        ProbeRunTermination::Exited if result.exit_code == Some(0) => {
            (ProbeReceiptStatus::Succeeded, None, Some(0), None)
        }
        ProbeRunTermination::Exited => (
            ProbeReceiptStatus::Failed,
            Some(probe_error(
                DebugProbeErrorCode::InfrastructureError,
                "probe exited non-zero",
            )),
            result.exit_code.filter(|code| *code != 0),
            result.signal,
        ),
        ProbeRunTermination::TimedOut => (
            ProbeReceiptStatus::TimedOut,
            Some(probe_error(
                DebugProbeErrorCode::TimedOut,
                "probe timed out",
            )),
            None,
            result.signal,
        ),
        ProbeRunTermination::Cancelled => (
            ProbeReceiptStatus::Cancelled,
            Some(probe_error(
                DebugProbeErrorCode::Cancelled,
                "probe was cancelled",
            )),
            None,
            result.signal,
        ),
        ProbeRunTermination::CleanupFailed => (
            ProbeReceiptStatus::Failed,
            Some(probe_error(
                DebugProbeErrorCode::ProcessCleanupFailed,
                "probe process cleanup failed",
            )),
            result.exit_code.filter(|code| *code != 0),
            result.signal,
        ),
        ProbeRunTermination::OutputLimitExceeded | ProbeRunTermination::InfrastructureError => (
            ProbeReceiptStatus::Failed,
            Some(probe_error(
                DebugProbeErrorCode::InfrastructureError,
                "probe execution infrastructure failed",
            )),
            result.exit_code.filter(|code| *code != 0),
            result.signal,
        ),
    };
    ProbeExecutionReceipt {
        artifact_refs: Vec::new(),
        duration_millis,
        error,
        exit_code,
        finished_at: finished_at.clone(),
        identity: intent.identity.clone(),
        output_bytes,
        output_truncated: result.output_truncated,
        plan_digest: intent.plan_digest.clone(),
        schema_version: 1,
        signal,
        started_at: started_at.clone(),
        status,
        timed_out: result.termination == ProbeRunTermination::TimedOut,
    }
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
    Ok(path)
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
        now: &Instant,
    ) -> Result<RoundClaim, ProbeSchedulerError> {
        let round_id = &plan.plan().authority.round_id.0;
        match self.replay_round(plan)? {
            RoundReplay::Terminal(receipt) => return Ok(RoundClaim::Terminal(receipt)),
            RoundReplay::Unresolved => return Ok(RoundClaim::Unresolved),
            RoundReplay::Missing => {}
        }

        let plan_bytes = serde_json::to_vec(plan.plan()).map_err(|_| journal_error())?;
        let transaction = self.connection.transaction().map_err(|_| journal_error())?;
        transaction
            .execute(
                "INSERT INTO probe_round
                   (round_id, plan_json, receipt_json, created_at, updated_at)
                 VALUES (?1, ?2, NULL, ?3, ?3)",
                params![round_id, plan_bytes, now.0],
            )
            .map_err(|_| journal_error())?;
        for probe in admitted {
            let ordinal = plan
                .probe_by_id(&probe.intent.identity.probe_id)
                .map(|probe| i64::try_from(probe.ordinal()).unwrap_or(i64::MAX))
                .ok_or_else(journal_error)?;
            let bytes = serde_json::to_vec(&probe.intent).map_err(|_| journal_error())?;
            transaction
                .execute(
                    "INSERT INTO probe_execution
                       (probe_execution_id, round_id, ordinal, intent_json, receipt_json,
                        created_at, updated_at)
                     VALUES (?1, ?2, ?3, ?4, NULL, ?5, ?5)",
                    params![
                        probe.intent.identity.probe_execution_id.0,
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
    ) -> Result<RoundReplay, ProbeSchedulerError> {
        let round_id = &plan.plan().authority.round_id.0;
        let plan_bytes = serde_json::to_vec(plan.plan()).map_err(|_| journal_error())?;
        let existing = self
            .connection
            .query_row(
                "SELECT plan_json, receipt_json FROM probe_round WHERE round_id = ?1",
                [round_id],
                |row| Ok((row.get::<_, Vec<u8>>(0)?, row.get::<_, Option<Vec<u8>>>(1)?)),
            )
            .optional()
            .map_err(|_| journal_error())?;
        let Some((stored_plan, stored_receipt)) = existing else {
            return Ok(RoundReplay::Missing);
        };
        if stored_plan != plan_bytes {
            return Err(stale_authority("probe round replay changed its plan bytes"));
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
