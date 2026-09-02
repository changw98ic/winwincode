// SPDX-License-Identifier: Apache-2.0

//! Standalone production composition for the Worker model `ExecutionPort`.
//!
//! Every entry reconstructs the same durable runtime from the canonical
//! database. Local and remote transports therefore share one typed core and
//! neither transport owns Provider, Credential, admission, or retry state.

use std::{
    collections::BTreeMap,
    fmt, fs,
    path::{Path, PathBuf},
};

use base64::{Engine as _, engine::general_purpose::STANDARD};
use serde::Serialize;
use sha2::{Digest, Sha256};
use winwincode_domain::{Instant, ModelExchangeId};
use winwincode_execution_port::{
    generated::{ExecutionPortMessage, ModelOpenMessage},
    transport::{
        AdapterError, EndpointSide, ExecutionPortCore, LocalWorkerAdapter, RemoteTransportAdapter,
        TypedFrame,
    },
};
use winwincode_storage::{
    ProductStateStorage, SqliteStorage, StorageError, WorkerHealth, WorkerSlotState,
};

use crate::model_execution_runtime::recover_terminal_model_execution_batches;
use crate::product_session_execution_application::{
    project_product_session_model_batch, reconcile_product_session_model_frames,
};
use crate::{
    ConfiguredModelRetryPlanAuthority, DurableEnterpriseQuotaAdmission,
    DurableExecutionPortContext, DurableExecutionPortDelegate, DurableExecutionPortError,
    DurableExecutionPortSupplement, DurableModelExchangeAuthority, DurableModelRetryContextSource,
    DurableModelRetryPreOpenPlanner, DurableProviderGatewayAdmission,
    DurableProviderRetrySettlement, HttpsSseProviderAdapter, HttpsSseProviderConfig,
    HttpsSseProviderError, HttpsSseProviderErrorKind, LocalSecretStoreAdapter, ModelAdmissionClock,
    ModelExecutionBatchReceipt, ModelExecutionOpenReceipt, ModelExecutionPortReceipt,
    ModelExecutionRuntime, ModelExecutionRuntimeError, ModelPolicyAuthorityPort, ModelRequestPool,
    ModelRequestPoolConfig, ModelRetryPlanAuthorityPort, ProviderAdapterError,
    ProviderAdapterInvocation, ProviderAdapterOpenReceipt, ProviderAdapterPort,
    ProviderAdmissionReservationConfig, ProviderFinishReason, ProviderGateway,
    ProviderGatewayIdentity, ProviderGatewayIdentityError, ProviderGatewayIdentityPort,
    ProviderGatewayOpenReceipt, ProviderGatewayTerminal, ProviderStreamControlAction,
    ProviderStreamConverter, ProviderStreamEvent, ProviderStreamFailure, ProviderStreamFailureKind,
    ProviderTokenUsage, ProviderToolIdentity, ProviderToolKind, ResolvedSecret,
    SystemModelAdmissionClock,
};

const LOOPBACK_RESPONSE: &str = "WinWinCode deterministic loopback response";
const PLANNER_PROTOCOL: &str = "winwincode.planner-solution.v1";
const VERIFICATION_PROTOCOL: &str = "winwincode.independent-verification-result.v1";
const STAGE_INPUT_MARKER: &str = "StrongFlow stageInput (canonical JSON):\n";
const REQUIRED_BEHAVIOR_MARKER: &str = "\n\nRequired behavior:";
const EXECUTOR_BEHAVIOR_MARKER: &str =
    "Apply the requested source change in the assigned checkout.";
const VERIFICATION_BEHAVIOR_MARKER: &str =
    "Use read-only commands against exactly stageInput.candidateRef.";

/// The deterministic local Provider has to honor the same output contract as
/// an actual model.  A Delivery planner is not allowed to complete with the
/// generic text response because the Worker intentionally validates its strict
/// Planner product before writing a terminal Activity.  Keep this profile
/// secret-free: only the criterion identities needed to build the deterministic
/// planner envelope are retained.
#[derive(Clone, Debug)]
enum LoopbackResponseProfile {
    PlainText,
    Planner {
        acceptance_criterion_ids: Vec<String>,
    },
    Executor {
        completed: bool,
    },
    Verification {
        completed: bool,
        /// The loopback Provider only emits a terminal verification product
        /// after the Worker has returned the direct command output.  Keep the
        /// observed command outcome attached to the profile so the model
        /// product cannot claim `pass` merely because a tool-output item
        /// exists.
        verification_outcome: LoopbackVerificationOutcome,
        evidence_type: &'static str,
        delivery_spec_id: String,
        delivery_spec_revision: u64,
        candidate_ref: String,
        acceptance_criterion_ids: Vec<String>,
    },
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum LoopbackVerificationOutcome {
    Succeeded,
    Failed,
    Unknown,
}

#[derive(Clone, Debug)]
struct LoopbackStageInput {
    delivery_spec_id: String,
    delivery_spec_revision: u64,
    candidate_ref: Option<String>,
    acceptance_criterion_ids: Vec<String>,
}

#[derive(Serialize)]
#[serde(rename_all = "snake_case")]
struct LoopbackVerificationResult {
    protocol: &'static str,
    delivery_spec_id: String,
    delivery_spec_revision: u64,
    candidate_ref: String,
    findings: Vec<LoopbackVerificationFinding>,
}

#[derive(Serialize)]
#[serde(rename_all = "snake_case")]
struct LoopbackVerificationFinding {
    finding_id: String,
    criterion_id: String,
    verdict: &'static str,
    explanation: &'static str,
    evidence_sources: Vec<LoopbackVerificationEvidenceSource>,
}

#[derive(Serialize)]
struct LoopbackVerificationEvidenceSource {
    #[serde(rename = "type")]
    evidence_type: &'static str,
    source_id: &'static str,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct LoopbackPlannerSolutionV1 {
    schema_version: u8,
    protocol: &'static str,
    solution: LoopbackPlannerSolution,
    architecture_diagram: LoopbackPlannerDiagram,
    process_diagram: LoopbackPlannerDiagram,
    risks: Vec<String>,
    unresolved_items: Vec<String>,
    task_proposals: Vec<LoopbackPlannerTaskProposal>,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct LoopbackPlannerSolution {
    id: &'static str,
    summary: &'static str,
    approach: Vec<&'static str>,
    components: Vec<LoopbackPlannerComponent>,
    connections: Vec<LoopbackPlannerConnection>,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct LoopbackPlannerComponent {
    id: &'static str,
    label: &'static str,
    responsibility: &'static str,
    kind: LoopbackPlannerComponentKind,
    trust_boundary: Option<&'static str>,
    unresolved: bool,
    repository_path_prefixes: Vec<&'static str>,
}

#[derive(Serialize)]
#[serde(rename_all = "kebab-case")]
enum LoopbackPlannerComponentKind {
    Component,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct LoopbackPlannerConnection {
    id: &'static str,
    from: &'static str,
    to: &'static str,
    label: &'static str,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct LoopbackPlannerDiagram {
    id: &'static str,
    kind: LoopbackPlannerDiagramKind,
    title: &'static str,
    nodes: Vec<LoopbackPlannerNode>,
    edges: Vec<LoopbackPlannerConnection>,
}

#[derive(Serialize)]
enum LoopbackPlannerDiagramKind {
    #[serde(rename = "system-architecture")]
    SystemArchitecture,
    #[serde(rename = "process-flow")]
    ProcessFlow,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct LoopbackPlannerNode {
    id: &'static str,
    label: &'static str,
    description: &'static str,
    kind: LoopbackPlannerNodeKind,
    trust_boundary: Option<&'static str>,
    unresolved: bool,
}

#[derive(Serialize)]
enum LoopbackPlannerNodeKind {
    #[serde(rename = "stage")]
    Stage,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct LoopbackPlannerTaskProposal {
    id: &'static str,
    title: &'static str,
    goal: &'static str,
    acceptance_criterion_ids: Vec<String>,
    blocked_by_task_ids: Vec<String>,
}

fn loopback_profile(message: &ModelOpenMessage) -> LoopbackResponseProfile {
    let Ok(bytes) = STANDARD.decode(&message.request.data_base64) else {
        return LoopbackResponseProfile::PlainText;
    };
    let Ok(request) = serde_json::from_slice::<serde_json::Value>(&bytes) else {
        return LoopbackResponseProfile::PlainText;
    };
    loopback_profile_from_request(&request)
}

fn loopback_profile_from_request(request: &serde_json::Value) -> LoopbackResponseProfile {
    let completed = contains_value_with_type(request, "function_call_output")
        || contains_value_with_type(request, "custom_tool_call_output");

    if find_text_value(request, EXECUTOR_BEHAVIOR_MARKER).is_some() {
        return LoopbackResponseProfile::Executor { completed };
    }

    if find_text_value(request, VERIFICATION_BEHAVIOR_MARKER).is_some() {
        let Some(stage_input) = stage_input_from_request(request) else {
            return LoopbackResponseProfile::PlainText;
        };
        let Some(candidate_ref) = stage_input.candidate_ref else {
            return LoopbackResponseProfile::PlainText;
        };
        // The production loopback emits direct shell-command evidence for
        // this verifier turn.  Keep the model response bound to that exact
        // event category so the Worker can resolve the source by identity;
        // the Server only routes the resulting typed frame.
        let evidence_type = "command";
        return LoopbackResponseProfile::Verification {
            completed,
            verification_outcome: loopback_verification_outcome(request),
            evidence_type,
            delivery_spec_id: stage_input.delivery_spec_id,
            delivery_spec_revision: stage_input.delivery_spec_revision,
            candidate_ref,
            acceptance_criterion_ids: stage_input.acceptance_criterion_ids,
        };
    }

    if find_text_value(request, PLANNER_PROTOCOL).is_none() {
        return LoopbackResponseProfile::PlainText;
    }
    let Some(stage_input) = stage_input_from_request(request) else {
        return LoopbackResponseProfile::PlainText;
    };
    if stage_input.acceptance_criterion_ids.is_empty() {
        LoopbackResponseProfile::PlainText
    } else {
        LoopbackResponseProfile::Planner {
            acceptance_criterion_ids: stage_input.acceptance_criterion_ids,
        }
    }
}

/// Reads the actual Worker-produced output for the deterministic verification
/// command.  The embedded Codex protocol serializes the command result as a
/// plain text `function_call_output`; the exit-code line is therefore the
/// durable bridge between the tool result and the Provider's structured
/// verification product.
fn loopback_verification_outcome(request: &serde_json::Value) -> LoopbackVerificationOutcome {
    let Some(output) = find_tool_output(request, "loopback-verification-command") else {
        return LoopbackVerificationOutcome::Unknown;
    };
    let Some(text) = tool_output_text(output) else {
        return LoopbackVerificationOutcome::Unknown;
    };
    let Some(exit_code) = parse_process_exit_code(text) else {
        return LoopbackVerificationOutcome::Unknown;
    };
    if exit_code == 0 {
        LoopbackVerificationOutcome::Succeeded
    } else {
        LoopbackVerificationOutcome::Failed
    }
}

fn find_tool_output<'value>(
    value: &'value serde_json::Value,
    call_id: &str,
) -> Option<&'value serde_json::Value> {
    match value {
        serde_json::Value::Object(values) => {
            let is_tool_output = values
                .get("type")
                .and_then(serde_json::Value::as_str)
                .is_some_and(|kind| {
                    matches!(kind, "function_call_output" | "custom_tool_call_output")
                });
            if is_tool_output
                && values.get("call_id").and_then(serde_json::Value::as_str) == Some(call_id)
            {
                return Some(value);
            }
            values
                .values()
                .find_map(|value| find_tool_output(value, call_id))
        }
        serde_json::Value::Array(values) => values
            .iter()
            .find_map(|value| find_tool_output(value, call_id)),
        _ => None,
    }
}

fn tool_output_text(output: &serde_json::Value) -> Option<&str> {
    let body = output.get("output")?;
    match body {
        serde_json::Value::String(text) => Some(text),
        serde_json::Value::Array(items) => items
            .iter()
            .find_map(|item| item.get("text").and_then(serde_json::Value::as_str)),
        _ => None,
    }
}

fn parse_process_exit_code(text: &str) -> Option<i64> {
    text.lines().find_map(|line| {
        ["Exit code: ", "Process exited with code "]
            .into_iter()
            .find_map(|marker| line.strip_prefix(marker))
            .and_then(|code| code.trim().parse().ok())
    })
}

fn stage_input_from_request(request: &serde_json::Value) -> Option<LoopbackStageInput> {
    let instructions = find_text_value(request, STAGE_INPUT_MARKER)?;
    let start = instructions
        .find(STAGE_INPUT_MARKER)?
        .checked_add(STAGE_INPUT_MARKER.len())?;
    let end = instructions[start..]
        .find(REQUIRED_BEHAVIOR_MARKER)?
        .checked_add(start)?;
    let stage_input = serde_json::from_str::<serde_json::Value>(&instructions[start..end]).ok()?;
    let delivery_spec_id = stage_input
        .get("deliverySpecId")
        .and_then(serde_json::Value::as_str)
        .filter(|value| !value.trim().is_empty())?
        .to_owned();
    let delivery_spec_revision = stage_input
        .get("deliverySpecRevision")
        .and_then(serde_json::Value::as_u64)?;
    let candidate_ref = stage_input
        .get("candidateRef")
        .and_then(serde_json::Value::as_str)
        .filter(|value| !value.trim().is_empty())
        .map(ToOwned::to_owned);
    let criteria = stage_input
        .get("acceptanceCriteria")
        .and_then(serde_json::Value::as_array)?;
    let mut acceptance_criterion_ids = Vec::with_capacity(criteria.len());
    for criterion in criteria {
        let id = criterion
            .get("criterionId")
            .and_then(serde_json::Value::as_str)
            .filter(|id| !id.trim().is_empty())?;
        if !acceptance_criterion_ids
            .iter()
            .any(|existing| existing == id)
        {
            acceptance_criterion_ids.push(id.to_owned());
        }
    }
    Some(LoopbackStageInput {
        delivery_spec_id,
        delivery_spec_revision,
        candidate_ref,
        acceptance_criterion_ids,
    })
}

fn find_text_value<'value>(value: &'value serde_json::Value, needle: &str) -> Option<&'value str> {
    match value {
        serde_json::Value::String(text) if text.contains(needle) => Some(text),
        serde_json::Value::Array(values) => values
            .iter()
            .find_map(|value| find_text_value(value, needle)),
        serde_json::Value::Object(values) => values
            .values()
            .find_map(|value| find_text_value(value, needle)),
        _ => None,
    }
}

fn contains_value_with_type(value: &serde_json::Value, expected: &str) -> bool {
    match value {
        serde_json::Value::Object(values) => {
            values
                .get("type")
                .and_then(serde_json::Value::as_str)
                .is_some_and(|actual| actual == expected)
                || values
                    .values()
                    .any(|value| contains_value_with_type(value, expected))
        }
        serde_json::Value::Array(values) => values
            .iter()
            .any(|value| contains_value_with_type(value, expected)),
        _ => false,
    }
}

fn loopback_response_for_profile(profile: &LoopbackResponseProfile) -> String {
    match profile {
        LoopbackResponseProfile::PlainText => LOOPBACK_RESPONSE.to_owned(),
        LoopbackResponseProfile::Planner {
            acceptance_criterion_ids,
        } => {
            let product = LoopbackPlannerSolutionV1 {
                schema_version: 1,
                protocol: PLANNER_PROTOCOL,
                solution: LoopbackPlannerSolution {
                    id: "solution:deterministic-loopback",
                    summary: "Apply the approved Delivery plan.",
                    approach: vec!["Apply the approved scope and run the exact checks."],
                    components: vec![LoopbackPlannerComponent {
                        id: "component:deterministic-loopback",
                        label: "Approved Delivery",
                        responsibility: "Represent the approved source change.",
                        kind: LoopbackPlannerComponentKind::Component,
                        trust_boundary: Some("repository"),
                        unresolved: false,
                        repository_path_prefixes: vec!["crates"],
                    }],
                    connections: vec![LoopbackPlannerConnection {
                        id: "connection:deterministic-loopback",
                        from: "platform:codex-core",
                        to: "component:deterministic-loopback",
                        label: "plans",
                    }],
                },
                architecture_diagram: LoopbackPlannerDiagram {
                    id: "diagram:deterministic-architecture",
                    kind: LoopbackPlannerDiagramKind::SystemArchitecture,
                    title: "Approved Delivery architecture",
                    nodes: vec![LoopbackPlannerNode {
                        id: "diagram:deterministic-architecture:stage",
                        label: "Delivery stage",
                        description: "Applies the approved source change.",
                        kind: LoopbackPlannerNodeKind::Stage,
                        trust_boundary: None,
                        unresolved: false,
                    }],
                    edges: Vec::new(),
                },
                process_diagram: LoopbackPlannerDiagram {
                    id: "diagram:deterministic-process",
                    kind: LoopbackPlannerDiagramKind::ProcessFlow,
                    title: "Approved Delivery process",
                    nodes: vec![LoopbackPlannerNode {
                        id: "diagram:deterministic-process:stage",
                        label: "Plan and verify",
                        description: "Plans and verifies the approved change.",
                        kind: LoopbackPlannerNodeKind::Stage,
                        trust_boundary: None,
                        unresolved: false,
                    }],
                    edges: Vec::new(),
                },
                risks: vec!["The exact check may expose a regression.".to_owned()],
                unresolved_items: Vec::new(),
                task_proposals: vec![LoopbackPlannerTaskProposal {
                    id: "dtk_00000000000000000000000001",
                    title: "Apply approved Delivery plan",
                    goal: "Apply the approved source change and run its checks.",
                    acceptance_criterion_ids: acceptance_criterion_ids.clone(),
                    blocked_by_task_ids: Vec::new(),
                }],
            };
            serde_json::to_string(&product).expect("serialize deterministic Planner response")
        }
        LoopbackResponseProfile::Executor { completed: true } => {
            "The requested stage action completed.".to_owned()
        }
        LoopbackResponseProfile::Verification {
            completed: true, ..
        } => loopback_verification_response(profile)
            .unwrap_or_else(|| "The requested stage action completed.".to_owned()),
        LoopbackResponseProfile::Executor { completed: false } => {
            // The command intentionally creates an untracked source file in
            // the Worker-owned detached checkout.  WorkerWorkspace stages
            // untracked files when it freezes the candidate, so this is a
            // real Git tree change rather than a Server-side product.
            String::new()
        }
        LoopbackResponseProfile::Verification {
            completed: false, ..
        } => String::new(),
    }
}

fn loopback_tool_call_for_profile(
    profile: &LoopbackResponseProfile,
) -> Option<(&'static str, &'static str)> {
    match profile {
        LoopbackResponseProfile::Executor { completed: false } => Some((
            "loopback-executor-change",
            "printf '%s\\n' 'deterministic StrongFlow candidate' > .winwincode-api-candidate; pwd; git status --porcelain=v1 --untracked-files=all",
        )),
        LoopbackResponseProfile::Verification {
            completed: false, ..
        } => Some((
            "loopback-verification-command",
            "test -s .winwincode-api-candidate",
        )),
        _ => None,
    }
}

fn loopback_verification_response(profile: &LoopbackResponseProfile) -> Option<String> {
    let LoopbackResponseProfile::Verification {
        completed: true,
        verification_outcome,
        evidence_type,
        delivery_spec_id,
        delivery_spec_revision,
        candidate_ref,
        acceptance_criterion_ids,
        ..
    } = profile
    else {
        return None;
    };
    let (verdict, explanation) = match verification_outcome {
        LoopbackVerificationOutcome::Succeeded => (
            "pass",
            "The observed verification command completed successfully.",
        ),
        LoopbackVerificationOutcome::Failed => (
            "fail",
            "The observed verification command exited with a non-zero code.",
        ),
        // A terminal verification product without a directly observed exit
        // code is not trusted.  Returning `fail` keeps the canonical product
        // shape while Delivery's authority still cross-checks the exact
        // command event and rejects any disagreement.
        LoopbackVerificationOutcome::Unknown => (
            "fail",
            "The verification command produced no directly observed exit code.",
        ),
    };
    let findings = acceptance_criterion_ids
        .iter()
        .map(|criterion_id| LoopbackVerificationFinding {
            finding_id: format!("finding:deterministic:{criterion_id}"),
            criterion_id: criterion_id.clone(),
            verdict,
            explanation,
            evidence_sources: vec![LoopbackVerificationEvidenceSource {
                evidence_type,
                source_id: "loopback-verification-command",
            }],
        })
        .collect();
    Some(
        serde_json::to_string(&LoopbackVerificationResult {
            protocol: VERIFICATION_PROTOCOL,
            delivery_spec_id: delivery_spec_id.clone(),
            delivery_spec_revision: *delivery_spec_revision,
            candidate_ref: candidate_ref.clone(),
            findings,
        })
        .expect("serialize deterministic verification response"),
    )
}

struct NoopModelExecutionCore;

impl ExecutionPortCore for NoopModelExecutionCore {
    type Output = ();
    type Error = std::convert::Infallible;

    fn accept(&mut self, _message: &ExecutionPortMessage) -> Result<Self::Output, Self::Error> {
        Ok(())
    }
}

/// Stable standalone composition failure categories.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum StandaloneModelExecutionErrorKind {
    InvalidConfiguration,
    DependencyUnavailable,
    Transport,
    Runtime,
    Shutdown,
}

/// Bounded error which never retains model input, Credential, or Provider text.
#[derive(Debug)]
pub struct StandaloneModelExecutionError {
    kind: StandaloneModelExecutionErrorKind,
}

impl StandaloneModelExecutionError {
    const fn new(kind: StandaloneModelExecutionErrorKind) -> Self {
        Self { kind }
    }

    #[must_use]
    pub const fn kind(&self) -> StandaloneModelExecutionErrorKind {
        self.kind
    }
}

impl fmt::Display for StandaloneModelExecutionError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("standalone model execution operation failed")
    }
}

impl std::error::Error for StandaloneModelExecutionError {}

/// Durable identity source joining one model envelope to Worker, lease, slot,
/// authenticated execution Job, and repository scope authority.
pub struct DurableProviderGatewayIdentitySource {
    data_directory: PathBuf,
    database_path: PathBuf,
}

impl DurableProviderGatewayIdentitySource {
    /// Opens and verifies the canonical product database.
    ///
    /// # Errors
    ///
    /// Returns `Unavailable` when the database cannot be opened.
    pub fn open(data_directory: impl AsRef<Path>) -> Result<Self, ProviderGatewayIdentityError> {
        let storage = SqliteStorage::open(data_directory.as_ref())
            .map_err(|_| ProviderGatewayIdentityError::unavailable())?;
        let database_path = storage.database_path().to_path_buf();
        Ok(Self {
            data_directory: data_directory.as_ref().to_path_buf(),
            database_path,
        })
    }

    #[must_use]
    pub fn database_path(&self) -> &Path {
        &self.database_path
    }
}

impl ProviderGatewayIdentityPort for DurableProviderGatewayIdentitySource {
    fn authorize(
        &self,
        message: &ModelOpenMessage,
    ) -> Result<ProviderGatewayIdentity, ProviderGatewayIdentityError> {
        let mut storage = SqliteStorage::open(&self.data_directory).map_err(|error| {
            if std::env::var_os("WWC_DEBUG_RUNTIME").is_some() {
                eprintln!("provider identity stage=open error={error:?}");
            }
            ProviderGatewayIdentityError::unavailable()
        })?;
        let (lease, worker) = {
            let registry = storage.execution_registry().map_err(|error| {
                if std::env::var_os("WWC_DEBUG_RUNTIME").is_some() {
                    eprintln!("provider identity stage=execution_registry error={error:?}");
                }
                ProviderGatewayIdentityError::unavailable()
            })?;
            let lease = registry
                .load_lease(&message.lease.job_id)
                .map_err(|error| {
                    if std::env::var_os("WWC_DEBUG_RUNTIME").is_some() {
                        eprintln!("provider identity stage=load_lease error={error:?}");
                    }
                    ProviderGatewayIdentityError::unavailable()
                })?
                .ok_or_else(ProviderGatewayIdentityError::denied)?;
            let worker = registry
                .load_worker(&message.lease.worker_id)
                .map_err(|error| {
                    if std::env::var_os("WWC_DEBUG_RUNTIME").is_some() {
                        eprintln!("provider identity stage=load_worker error={error:?}");
                    }
                    ProviderGatewayIdentityError::unavailable()
                })?
                .ok_or_else(ProviderGatewayIdentityError::denied)?;
            (lease, worker)
        };
        let message_attempt = u64::try_from(message.lease.attempt)
            .map_err(|_| ProviderGatewayIdentityError::denied())?;
        if lease.lease_id != message.lease.lease_id
            || lease.worker_id != message.lease.worker_id
            || lease.worker_instance_id != message.lease.worker_instance_id
            || lease.attempt != message_attempt
            || lease.fencing_token != message.lease.fencing_token
            || lease.issued_at != message.lease.issued_at
            || lease.expires_at != message.lease.expires_at
            || message.sent_at.0 < lease.issued_at.0
            || message.sent_at.0 >= lease.expires_at.0
            || worker.worker_instance_id != message.lease.worker_instance_id
            || worker.health != WorkerHealth::Healthy
        {
            return Err(ProviderGatewayIdentityError::denied());
        }
        let slot = storage
            .worker_session_slots()
            .map_err(|error| {
                if std::env::var_os("WWC_DEBUG_RUNTIME").is_some() {
                    eprintln!("provider identity stage=worker_session_slots error={error:?}");
                }
                ProviderGatewayIdentityError::unavailable()
            })?
            .load(&message.worker_session_id)
            .map_err(|error| {
                if std::env::var_os("WWC_DEBUG_RUNTIME").is_some() {
                    eprintln!("provider identity stage=load_slot error={error:?}");
                }
                ProviderGatewayIdentityError::unavailable()
            })?
            .ok_or_else(ProviderGatewayIdentityError::denied)?;
        if slot.state != WorkerSlotState::Running
            || slot.authority.worker_id != message.lease.worker_id
            || slot.authority.worker_instance_id != message.lease.worker_instance_id
            || slot.authority.worker_session_id != message.worker_session_id
            || slot.authority.codex_thread_id != message.session_identity.codex_thread_id
            || slot.authority.job_id != message.lease.job_id
            || slot.authority.lease_id != message.lease.lease_id
            || slot.authority.attempt != message_attempt
            || slot.authority.fencing_token != message.lease.fencing_token
            || message.worker_session_id != message.session_identity.worker_session_id
        {
            return Err(ProviderGatewayIdentityError::denied());
        }
        crate::model_retry_planner::provider_gateway_identity(&storage, message).map_err(|error| {
            if std::env::var_os("WWC_DEBUG_RUNTIME").is_some() {
                eprintln!("provider identity stage=planner kind={:?}", error.kind());
            }
            if error.kind() == crate::ModelRetryPlannerErrorKind::Storage {
                ProviderGatewayIdentityError::unavailable()
            } else {
                ProviderGatewayIdentityError::denied()
            }
        })
    }
}

/// Explicit offline Provider implementation used by local production and
/// deterministic contract gates. It performs no network access and never
/// copies the model input or Credential into retained state.
#[derive(Clone)]
pub struct DeterministicLoopbackProviderAdapter {
    provider_id: String,
}

impl DeterministicLoopbackProviderAdapter {
    /// Creates a bounded explicit loopback Provider.
    ///
    /// # Errors
    ///
    /// Rejects malformed Provider identifiers.
    pub fn try_new(provider_id: String) -> Result<Self, ProviderAdapterError> {
        if !valid_token(&provider_id, 128) {
            return Err(ProviderAdapterError::protocol());
        }
        Ok(Self { provider_id })
    }

    #[must_use]
    pub fn provider_id(&self) -> &str {
        &self.provider_id
    }
}

impl ProviderAdapterPort for DeterministicLoopbackProviderAdapter {
    fn provider_id(&self) -> &str {
        &self.provider_id
    }

    fn open(
        &self,
        invocation: &ProviderAdapterInvocation<'_>,
        credential: &ResolvedSecret,
    ) -> Result<ProviderAdapterOpenReceipt, ProviderAdapterError> {
        if invocation.content_type() != "application/json"
            || invocation.payload().is_empty()
            || credential.expose().is_empty()
        {
            return Err(ProviderAdapterError::rejected());
        }
        serde_json::from_slice::<serde_json::Value>(invocation.payload())
            .map_err(|_| ProviderAdapterError::rejected())?;
        ProviderAdapterOpenReceipt::try_new(invocation.adapter_request_id().to_owned())
    }

    fn control(
        &self,
        _model_exchange_id: &ModelExchangeId,
        adapter_request_id: &str,
        _action: ProviderStreamControlAction,
    ) -> Result<(), ProviderAdapterError> {
        if !valid_token(adapter_request_id, 200) {
            return Err(ProviderAdapterError::protocol());
        }
        Ok(())
    }
}

/// Configuration for the one standalone model runtime.
#[derive(Clone)]
pub enum StandaloneProviderConfig {
    /// Deterministic provider used by offline production and release gates.
    Loopback { provider_id: String },
    /// Verified external HTTPS/SSE provider.
    HttpsSse(HttpsSseProviderConfig),
}

impl fmt::Debug for StandaloneProviderConfig {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Loopback { provider_id } => formatter
                .debug_struct("StandaloneProviderConfig::Loopback")
                .field("provider_id", provider_id)
                .finish(),
            Self::HttpsSse(config) => formatter
                .debug_struct("StandaloneProviderConfig::HttpsSse")
                .field("provider_id", &config.provider_id())
                .field("endpoint", &"[REDACTED]")
                .finish(),
        }
    }
}

/// Configuration for the one standalone model runtime.
pub struct StandaloneModelExecutionConfig {
    pub data_directory: PathBuf,
    pub secret_directory: PathBuf,
    pub providers: Vec<StandaloneProviderConfig>,
    pub admission: ProviderAdmissionReservationConfig,
    pub pool: ModelRequestPoolConfig,
    pub policy: Box<dyn ModelPolicyAuthorityPort>,
    pub retry_policy: Box<dyn ModelRetryPlanAuthorityPort>,
}

/// Production composition root used by both local and remote Worker adapters.
pub struct StandaloneModelExecutionApplication {
    data_directory: PathBuf,
    database_path: PathBuf,
    secrets: LocalSecretStoreAdapter,
    identity: DurableProviderGatewayIdentitySource,
    loopback: BTreeMap<String, DeterministicLoopbackProviderAdapter>,
    loopback_profiles: BTreeMap<String, LoopbackResponseProfile>,
    https_sse: BTreeMap<String, HttpsSseProviderAdapter>,
    admission: ProviderAdmissionReservationConfig,
    pool: ModelRequestPoolConfig,
    policy: Box<dyn ModelPolicyAuthorityPort>,
    retry_policy: Box<dyn ModelRetryPlanAuthorityPort>,
    clock: Box<dyn ModelAdmissionClock>,
}

impl StandaloneModelExecutionApplication {
    /// Opens the complete offline production composition with the system clock.
    ///
    /// # Errors
    ///
    /// Rejects overlapping data/secret roots, invalid limits, or unavailable
    /// durable dependencies.
    pub fn open(
        config: StandaloneModelExecutionConfig,
    ) -> Result<Self, StandaloneModelExecutionError> {
        Self::open_with_clock(config, Box::new(SystemModelAdmissionClock))
    }

    /// Opens the same composition with an injected authoritative clock.
    ///
    /// # Errors
    ///
    /// Returns the same bounded configuration/dependency errors as [`Self::open`].
    pub fn open_with_clock(
        config: StandaloneModelExecutionConfig,
        clock: Box<dyn ModelAdmissionClock>,
    ) -> Result<Self, StandaloneModelExecutionError> {
        let pool = ModelRequestPool::new(config.pool).map_err(|_| {
            StandaloneModelExecutionError::new(
                StandaloneModelExecutionErrorKind::InvalidConfiguration,
            )
        })?;
        drop(pool);
        let configured = configured_providers(config.providers)?;
        let mut storage = SqliteStorage::open(&config.data_directory).map_err(|_| {
            StandaloneModelExecutionError::new(
                StandaloneModelExecutionErrorKind::DependencyUnavailable,
            )
        })?;
        let database_path = storage.database_path().to_path_buf();
        for batch in recover_terminal_model_execution_batches(&mut storage, config.pool)
            .map_err(|_| dependency_unavailable())?
        {
            project_product_session_model_batch(&mut storage, &batch)
                .map_err(|_| dependency_unavailable())?;
        }
        reconcile_product_session_model_frames(&mut storage)
            .map_err(|_| dependency_unavailable())?;
        drop(storage);
        let secrets = LocalSecretStoreAdapter::open(&config.secret_directory).map_err(|_| {
            StandaloneModelExecutionError::new(
                StandaloneModelExecutionErrorKind::DependencyUnavailable,
            )
        })?;
        let data_root = fs::canonicalize(&config.data_directory).map_err(|_| {
            StandaloneModelExecutionError::new(
                StandaloneModelExecutionErrorKind::DependencyUnavailable,
            )
        })?;
        let secret_root = fs::canonicalize(&config.secret_directory).map_err(|_| {
            StandaloneModelExecutionError::new(
                StandaloneModelExecutionErrorKind::DependencyUnavailable,
            )
        })?;
        if data_root == secret_root
            || data_root.starts_with(&secret_root)
            || secret_root.starts_with(&data_root)
        {
            return Err(StandaloneModelExecutionError::new(
                StandaloneModelExecutionErrorKind::InvalidConfiguration,
            ));
        }
        let identity =
            DurableProviderGatewayIdentitySource::open(&config.data_directory).map_err(|_| {
                StandaloneModelExecutionError::new(
                    StandaloneModelExecutionErrorKind::DependencyUnavailable,
                )
            })?;
        if identity.database_path() != database_path {
            return Err(StandaloneModelExecutionError::new(
                StandaloneModelExecutionErrorKind::InvalidConfiguration,
            ));
        }
        Ok(Self {
            data_directory: config.data_directory,
            database_path,
            secrets,
            identity,
            loopback: configured.loopback,
            loopback_profiles: BTreeMap::new(),
            https_sse: configured.https_sse,
            admission: config.admission,
            pool: config.pool,
            policy: config.policy,
            retry_policy: config.retry_policy,
            clock,
        })
    }

    #[must_use]
    pub fn database_path(&self) -> &Path {
        &self.database_path
    }

    /// Accepts one local typed Worker frame through the canonical runtime.
    ///
    /// # Errors
    ///
    /// Fails closed for transport, authority, runtime, or shutdown errors.
    pub fn accept_local(
        &mut self,
        frame: &TypedFrame,
    ) -> Result<ModelExecutionPortReceipt, StandaloneModelExecutionError> {
        self.remember_loopback_profile(frame.message());
        self.with_runtime(|runtime| {
            LocalWorkerAdapter::new(runtime, EndpointSide::ControlPlane)
                .accept(frame)
                .map_err(|error| adapter_error(&error))
        })
    }

    /// Accepts one remote canonical JSON Worker frame through the same core.
    ///
    /// # Errors
    ///
    /// Fails closed before the core for malformed/direction-invalid frames.
    pub fn accept_remote(
        &mut self,
        bytes: &[u8],
    ) -> Result<ModelExecutionPortReceipt, StandaloneModelExecutionError> {
        if let Ok(frame) = RemoteTransportAdapter::<NoopModelExecutionCore>::decode(bytes) {
            self.remember_loopback_profile(frame.message());
        }
        self.with_runtime(|runtime| {
            RemoteTransportAdapter::new(runtime, EndpointSide::ControlPlane)
                .accept(bytes)
                .map_err(|error| adapter_error(&error))
        })
    }

    /// Emits the deterministic offline Provider completion through the same
    /// durable stream and terminal settlement path used by network Providers.
    ///
    /// # Errors
    ///
    /// Rejects a foreign Provider receipt or any conversion/runtime failure.
    pub fn complete_loopback(
        &mut self,
        open: &ProviderGatewayOpenReceipt,
        sent_at: &Instant,
    ) -> Result<ModelExecutionBatchReceipt, StandaloneModelExecutionError> {
        let batch = self.complete_loopback_before_projection(open, sent_at)?;
        self.project_product_session_batch(&batch)?;
        Ok(batch)
    }

    /// Completes one opened local model exchange with the provider adapter
    /// selected at startup. The returned batch is the canonical source of
    /// CP-to-Worker `model.chunk` frames; this method does not manufacture a
    /// terminal Job outcome.
    ///
    /// # Errors
    ///
    /// Returns the same bounded Provider/runtime errors as the selected
    /// production adapter.
    pub fn complete_local(
        &mut self,
        open: &ProviderGatewayOpenReceipt,
        sent_at: &Instant,
    ) -> Result<ModelExecutionBatchReceipt, StandaloneModelExecutionError> {
        if self.loopback.contains_key(&open.route.provider_id) {
            self.complete_loopback(open, sent_at)
        } else if self.https_sse.contains_key(&open.route.provider_id) {
            self.complete_https_sse(open, sent_at)
        } else {
            Err(invalid_configuration())
        }
    }

    fn complete_loopback_before_projection(
        &mut self,
        open: &ProviderGatewayOpenReceipt,
        sent_at: &Instant,
    ) -> Result<ModelExecutionBatchReceipt, StandaloneModelExecutionError> {
        self.loopback
            .get(&open.route.provider_id)
            .ok_or_else(invalid_configuration)?;
        let profile = self
            .loopback_profiles
            .get(&open.model_exchange_id.0)
            .cloned()
            .unwrap_or(LoopbackResponseProfile::PlainText);
        let usage = ProviderTokenUsage {
            input_tokens: 10,
            cached_input_tokens: 0,
            cache_write_input_tokens: 0,
            output_tokens: 5,
            reasoning_output_tokens: 0,
        };
        let mut converter = ProviderStreamConverter::from_gateway_receipt(open);
        let response_id = format!(
            "loopback_{:x}",
            Sha256::digest(open.adapter_request_id.as_bytes())
        );
        let mut events = vec![ProviderStreamEvent::ResponseStarted {
            provider_response_id: response_id,
        }];
        let finish_reason =
            if let Some((provider_call_id, command)) = loopback_tool_call_for_profile(&profile) {
                let identity = ProviderToolIdentity::try_new(
                    ProviderToolKind::Function,
                    "shell_command".to_owned(),
                    Some("functions".to_owned()),
                )
                .map_err(|_| runtime_failure())?;
                let arguments = serde_json::json!({
                    "command": command,
                    "workdir": ".",
                })
                .to_string();
                events.push(ProviderStreamEvent::ToolCallStarted {
                    index: 0,
                    provider_call_id: provider_call_id.to_owned(),
                    identity,
                });
                events.push(ProviderStreamEvent::ToolCallArgumentsDelta {
                    index: 0,
                    provider_call_id: provider_call_id.to_owned(),
                    delta: arguments,
                });
                events.push(ProviderStreamEvent::ToolCallEnded {
                    index: 0,
                    provider_call_id: provider_call_id.to_owned(),
                });
                ProviderFinishReason::ToolCalls
            } else {
                let response = loopback_verification_response(&profile)
                    .unwrap_or_else(|| loopback_response_for_profile(&profile));
                events.extend([
                    ProviderStreamEvent::TextStarted { index: 0 },
                    ProviderStreamEvent::TextDelta {
                        index: 0,
                        delta: response,
                    },
                    ProviderStreamEvent::TextEnded { index: 0 },
                ]);
                ProviderFinishReason::Stop
            };
        events.push(ProviderStreamEvent::Usage(usage));
        events.push(ProviderStreamEvent::Finished(finish_reason));
        let mut frames = Vec::new();
        for event in events {
            frames.extend(converter.ingest(event).map_err(|_| {
                StandaloneModelExecutionError::new(StandaloneModelExecutionErrorKind::Runtime)
            })?);
        }
        self.with_runtime(|runtime| {
            runtime
                .offer_provider_batch(
                    &open.model_exchange_id,
                    &frames,
                    Some(ProviderGatewayTerminal::Completed {
                        usage,
                        actual_cost_micros: 10,
                    }),
                    sent_at,
                )
                .map_err(|_| runtime_failure())
        })
    }

    fn remember_loopback_profile(&mut self, message: &ExecutionPortMessage) {
        let ExecutionPortMessage::ModelOpenMessage(open) = message else {
            return;
        };
        self.loopback_profiles
            .insert(open.model_exchange_id.0.clone(), loopback_profile(open));
    }

    /// Persists one complete loopback batch before the ProductSession
    /// projection boundary, allowing restart fault injection in contract tests.
    ///
    /// # Errors
    ///
    /// Returns the same bounded errors as [`Self::complete_loopback`].
    #[cfg(feature = "test-support")]
    #[doc(hidden)]
    pub fn complete_loopback_before_product_session_projection_for_test(
        &mut self,
        open: &ProviderGatewayOpenReceipt,
        sent_at: &Instant,
    ) -> Result<ModelExecutionBatchReceipt, StandaloneModelExecutionError> {
        self.complete_loopback_before_projection(open, sent_at)
    }

    /// Applies the private sealed ProductSession projection boundary to a test
    /// receipt.
    ///
    /// # Errors
    ///
    /// Rejects a changed receipt or any durable Chat projection failure.
    #[cfg(feature = "test-support")]
    #[doc(hidden)]
    pub fn project_product_session_batch_for_test(
        &self,
        batch: &ModelExecutionBatchReceipt,
    ) -> Result<(), StandaloneModelExecutionError> {
        self.project_product_session_batch(batch)
    }

    /// Drains one verified external HTTPS/SSE response and offers its canonical
    /// frames through the same durable runtime as the local and remote ports.
    ///
    /// # Errors
    ///
    /// Fails closed for a foreign Provider, malformed/oversized stream,
    /// credential leakage, flow control, or durable terminal settlement.
    pub fn complete_https_sse(
        &mut self,
        open: &ProviderGatewayOpenReceipt,
        sent_at: &Instant,
    ) -> Result<ModelExecutionBatchReceipt, StandaloneModelExecutionError> {
        let adapter = self
            .https_sse
            .get(&open.route.provider_id)
            .ok_or_else(invalid_configuration)?;
        let (frames, terminal) = match adapter.drain_canonical(open) {
            Ok(completion) => (completion.frames, completion.terminal),
            Err(error) if error.kind() == HttpsSseProviderErrorKind::Paused => {
                return Err(runtime_failure());
            }
            Err(error) => external_failure(open, &error)?,
        };
        let batch = self.with_runtime(|runtime| {
            runtime
                .offer_provider_batch(&open.model_exchange_id, &frames, Some(terminal), sent_at)
                .map_err(|_| runtime_failure())
        })?;
        self.project_product_session_batch(&batch)?;
        Ok(batch)
    }

    fn project_product_session_batch(
        &self,
        batch: &ModelExecutionBatchReceipt,
    ) -> Result<(), StandaloneModelExecutionError> {
        let mut storage = self.open_product_storage()?;
        project_product_session_model_batch(&mut storage, batch).map_err(|_| runtime_failure())
    }

    fn open_product_storage(&self) -> Result<SqliteStorage, StandaloneModelExecutionError> {
        SqliteStorage::open(&self.data_directory).map_err(|_| {
            StandaloneModelExecutionError::new(
                StandaloneModelExecutionErrorKind::DependencyUnavailable,
            )
        })
    }

    fn with_runtime<T>(
        &mut self,
        operation: impl FnOnce(
            &mut ModelExecutionRuntime<'_, '_>,
        ) -> Result<T, StandaloneModelExecutionError>,
    ) -> Result<T, StandaloneModelExecutionError> {
        let mut gateway_storage = self.open_product_storage()?;
        let admission_storage = self.open_product_storage()?;
        let enterprise_quota_storage = self.open_product_storage()?;
        let contexts = DurableModelRetryContextSource::open(&self.data_directory)
            .map_err(|_| dependency_unavailable())?;
        let settlement = DurableProviderRetrySettlement::open(&self.data_directory, &contexts)
            .map_err(|_| dependency_unavailable())?;
        let mut admission = DurableProviderGatewayAdmission::new(
            admission_storage,
            &*self.clock,
            &*self.policy,
            self.admission,
        );
        let mut enterprise_quota = DurableEnterpriseQuotaAdmission::new(enterprise_quota_storage);
        let mut enterprise_policy =
            crate::DurableProviderPolicyEnforcement::open(&self.data_directory)
                .map_err(|_| dependency_unavailable())?;
        let mut planner =
            DurableModelRetryPreOpenPlanner::open(&self.data_directory, &*self.retry_policy)
                .map_err(|_| dependency_unavailable())?;
        let exchanges = DurableModelExchangeAuthority::open(&self.data_directory)
            .map_err(|_| dependency_unavailable())?;
        let mut pool = ModelRequestPool::new(self.pool).map_err(|_| {
            StandaloneModelExecutionError::new(
                StandaloneModelExecutionErrorKind::InvalidConfiguration,
            )
        })?;
        if gateway_storage.database_path() != self.database_path
            || admission.database_path() != self.database_path
            || enterprise_quota.database_path() != self.database_path
            || enterprise_policy.database_path() != self.database_path
            || contexts.database_path() != self.database_path
            || planner.database_path() != self.database_path
            || exchanges.database_path() != self.database_path
        {
            return Err(StandaloneModelExecutionError::new(
                StandaloneModelExecutionErrorKind::InvalidConfiguration,
            ));
        }
        let mut gateway = ProviderGateway::new(
            &mut gateway_storage,
            &self.secrets,
            &self.identity,
            &settlement,
            &mut admission,
        );
        for adapter in self.loopback.values() {
            gateway
                .register_adapter(Box::new(adapter.clone()))
                .map_err(|_| invalid_configuration())?;
        }
        for adapter in self.https_sse.values() {
            gateway
                .register_adapter(Box::new(adapter.clone()))
                .map_err(|_| invalid_configuration())?;
        }
        let result = {
            let mut runtime = ModelExecutionRuntime::new_with_enterprise_controls(
                &exchanges,
                &mut planner,
                &contexts,
                &mut gateway,
                &mut pool,
                &mut enterprise_quota,
                &mut enterprise_policy,
            );
            operation(&mut runtime)
        };
        drop(gateway);
        let shutdown = exchanges
            .close()
            .map_err(|_| shutdown_error())
            .and_then(|()| planner.close().map_err(|_| shutdown_error()))
            .and_then(|()| admission.close().map_err(|_| shutdown_error()))
            .and_then(|()| enterprise_quota.close().map_err(|_| shutdown_error()))
            .and_then(|()| enterprise_policy.close().map_err(|_| shutdown_error()))
            .and_then(|()| settlement.close().map_err(|_| shutdown_error()))
            .and_then(|()| contexts.close().map_err(|_| shutdown_error()))
            .and_then(|()| {
                Box::new(gateway_storage)
                    .close()
                    .map_err(|_| shutdown_error())
            });
        match (result, shutdown) {
            (Err(error), _) | (Ok(_), Err(error)) => Err(error),
            (Ok(value), Ok(())) => Ok(value),
        }
    }
}

struct ConfiguredProviderAdapters {
    loopback: BTreeMap<String, DeterministicLoopbackProviderAdapter>,
    https_sse: BTreeMap<String, HttpsSseProviderAdapter>,
}

fn configured_providers(
    providers: Vec<StandaloneProviderConfig>,
) -> Result<ConfiguredProviderAdapters, StandaloneModelExecutionError> {
    if providers.is_empty() {
        return Err(invalid_configuration());
    }
    let mut loopback = BTreeMap::new();
    let mut https_sse = BTreeMap::new();
    for provider in providers {
        let (provider_id, loopback_adapter, https_adapter) = match provider {
            StandaloneProviderConfig::Loopback { provider_id } => {
                let adapter = DeterministicLoopbackProviderAdapter::try_new(provider_id.clone())
                    .map_err(|_| invalid_configuration())?;
                (provider_id, Some(adapter), None)
            }
            StandaloneProviderConfig::HttpsSse(config) => {
                let provider_id = config.provider_id().to_owned();
                let adapter = HttpsSseProviderAdapter::try_new(config)
                    .map_err(|_| invalid_configuration())?;
                (provider_id, None, Some(adapter))
            }
        };
        if loopback.contains_key(&provider_id) || https_sse.contains_key(&provider_id) {
            return Err(invalid_configuration());
        }
        if let Some(adapter) = loopback_adapter {
            loopback.insert(provider_id, adapter);
        } else if let Some(adapter) = https_adapter {
            https_sse.insert(provider_id, adapter);
        }
    }
    Ok(ConfiguredProviderAdapters {
        loopback,
        https_sse,
    })
}

fn external_failure(
    open: &ProviderGatewayOpenReceipt,
    error: &HttpsSseProviderError,
) -> Result<
    (
        Vec<crate::CanonicalModelStreamFrame>,
        ProviderGatewayTerminal,
    ),
    StandaloneModelExecutionError,
> {
    let failure = ProviderStreamFailure::new(match error.kind() {
        HttpsSseProviderErrorKind::RateLimited => ProviderStreamFailureKind::RateLimit,
        HttpsSseProviderErrorKind::Rejected => ProviderStreamFailureKind::InvalidRequest,
        HttpsSseProviderErrorKind::Protocol
        | HttpsSseProviderErrorKind::SizeLimit
        | HttpsSseProviderErrorKind::CredentialLeak => ProviderStreamFailureKind::Unknown,
        HttpsSseProviderErrorKind::InvalidConfiguration
        | HttpsSseProviderErrorKind::IdentityConflict
        | HttpsSseProviderErrorKind::Unavailable
        | HttpsSseProviderErrorKind::Transport
        | HttpsSseProviderErrorKind::Paused => ProviderStreamFailureKind::Transport,
    });
    let mut converter = ProviderStreamConverter::from_gateway_receipt(open);
    let frames = converter
        .ingest(ProviderStreamEvent::Failed(failure))
        .map_err(|_| runtime_failure())?;
    Ok((frames, error.failure_terminal()))
}

const fn invalid_configuration() -> StandaloneModelExecutionError {
    StandaloneModelExecutionError::new(StandaloneModelExecutionErrorKind::InvalidConfiguration)
}

const fn dependency_unavailable() -> StandaloneModelExecutionError {
    StandaloneModelExecutionError::new(StandaloneModelExecutionErrorKind::DependencyUnavailable)
}

const fn runtime_failure() -> StandaloneModelExecutionError {
    StandaloneModelExecutionError::new(StandaloneModelExecutionErrorKind::Runtime)
}

fn valid_token(value: &str, max_len: usize) -> bool {
    !value.is_empty()
        && value.len() <= max_len
        && value.trim() == value
        && !value.chars().any(char::is_control)
}

fn adapter_error(
    error: &AdapterError<ModelExecutionRuntimeError>,
) -> StandaloneModelExecutionError {
    if std::env::var_os("WWC_DEBUG_RUNTIME").is_some() {
        eprintln!("standalone model adapter error: {error:?}");
    }
    match error {
        AdapterError::Frame(_) => {
            StandaloneModelExecutionError::new(StandaloneModelExecutionErrorKind::Transport)
        }
        AdapterError::Core(_) => runtime_failure(),
    }
}

const fn shutdown_error() -> StandaloneModelExecutionError {
    StandaloneModelExecutionError::new(StandaloneModelExecutionErrorKind::Shutdown)
}

impl DurableExecutionPortDelegate for StandaloneModelExecutionApplication {
    fn accept(
        &mut self,
        _context: DurableExecutionPortContext<'_>,
        supplement: DurableExecutionPortSupplement<'_>,
    ) -> Result<Vec<ExecutionPortMessage>, DurableExecutionPortError> {
        let DurableExecutionPortSupplement::JobScopedWorkerMessage { message, .. } = supplement
        else {
            return Err(DurableExecutionPortError::UnsupportedMessage);
        };
        let frame = TypedFrame::new(
            winwincode_execution_port::transport::FrameDirection::WorkerToControlPlane,
            message.clone(),
        )
        .map_err(|_| model_delegate_failure())?;
        match message {
            ExecutionPortMessage::ModelOpenMessage(open) => {
                let receipt = self
                    .accept_local(&frame)
                    .map_err(|_| model_delegate_failure())?;
                let ModelExecutionPortReceipt::Opened(ModelExecutionOpenReceipt::Opened {
                    gateway,
                    ..
                }) = receipt
                else {
                    return Ok(Vec::new());
                };
                let batch = self
                    .complete_local(&gateway, &open.sent_at)
                    .map_err(|_| model_delegate_failure())?;
                Ok(batch
                    .chunks
                    .into_iter()
                    .map(ExecutionPortMessage::ModelChunkMessage)
                    .collect())
            }
            ExecutionPortMessage::ModelAckMessage(_) => {
                self.accept_local(&frame)
                    .map_err(|_| model_delegate_failure())?;
                Ok(Vec::new())
            }
            _ => Err(DurableExecutionPortError::UnsupportedMessage),
        }
    }
}

fn model_delegate_failure() -> DurableExecutionPortError {
    DurableExecutionPortError::Storage(StorageError::adapter(
        "standalone model execution delegate failed",
    ))
}

/// Default bounded retry policy for explicit local loopback deployments.
///
/// # Errors
///
/// Returns a bounded planner error if the built-in policy constants violate
/// the canonical retry-plan contract.
pub fn local_loopback_retry_policy()
-> Result<ConfiguredModelRetryPlanAuthority, crate::ModelRetryPlannerError> {
    ConfiguredModelRetryPlanAuthority::try_new(
        "winwincode.local-loopback.retry.v1".to_owned(),
        1,
        1,
    )
}

#[cfg(test)]
mod verification_tests {
    use serde_json::{Value, json};

    use super::{
        LoopbackResponseProfile, LoopbackVerificationOutcome, REQUIRED_BEHAVIOR_MARKER,
        STAGE_INPUT_MARKER, VERIFICATION_BEHAVIOR_MARKER, loopback_profile_from_request,
        loopback_verification_response,
    };

    fn request_with_output(output: Option<Value>) -> Value {
        let stage_input = json!({
            "deliverySpecId": "spec-test",
            "deliverySpecRevision": 1,
            "candidateRef": "git-candidate:sha256:test",
            "acceptanceCriteria": [{"criterionId": "criterion-test"}],
        });
        let mut input = vec![json!({
            "type": "message",
            "role": "user",
            "content": [{"type": "input_text", "text": "verify"}],
        })];
        if let Some(output) = output {
            input.push(output);
        }
        json!({
            "instructions": format!(
                "{STAGE_INPUT_MARKER}{stage_input}{REQUIRED_BEHAVIOR_MARKER}\n{VERIFICATION_BEHAVIOR_MARKER}"
            ),
            "input": input,
        })
    }

    fn output_item(text: &str) -> Value {
        json!({
            "type": "function_call_output",
            "call_id": "loopback-verification-command",
            "output": text,
        })
    }

    #[test]
    fn verification_profile_binds_pass_to_zero_exit_code() {
        let profile = loopback_profile_from_request(&request_with_output(Some(output_item(
            "Exit code: 0\nWall time: 1.0s\nOutput:\nok",
        ))));
        let LoopbackResponseProfile::Verification {
            completed,
            verification_outcome,
            ..
        } = profile
        else {
            panic!("expected verification profile");
        };
        assert!(completed);
        assert_eq!(verification_outcome, LoopbackVerificationOutcome::Succeeded);
    }

    #[test]
    fn verification_profile_binds_fail_to_nonzero_exit_code() {
        let profile = loopback_profile_from_request(&request_with_output(Some(output_item(
            "Exit code: 124\nWall time: 1.0s\nOutput:\ntimeout",
        ))));
        let LoopbackResponseProfile::Verification {
            completed,
            verification_outcome,
            ..
        } = profile
        else {
            panic!("expected verification profile");
        };
        assert!(completed);
        assert_eq!(verification_outcome, LoopbackVerificationOutcome::Failed);
        let response = loopback_verification_response(&LoopbackResponseProfile::Verification {
            completed: true,
            verification_outcome,
            evidence_type: "command",
            delivery_spec_id: "spec-test".to_owned(),
            delivery_spec_revision: 1,
            candidate_ref: "git-candidate:sha256:test".to_owned(),
            acceptance_criterion_ids: vec!["criterion-test".to_owned()],
        })
        .expect("verification response");
        let response: Value = serde_json::from_str(&response).expect("JSON response");
        assert_eq!(response["findings"][0]["verdict"], "fail");
    }

    #[test]
    fn verification_profile_fails_closed_without_an_exit_code() {
        let profile = loopback_profile_from_request(&request_with_output(Some(output_item(
            "command output",
        ))));
        let LoopbackResponseProfile::Verification {
            completed,
            verification_outcome,
            ..
        } = profile
        else {
            panic!("expected verification profile");
        };
        assert!(completed);
        assert_eq!(verification_outcome, LoopbackVerificationOutcome::Unknown);
    }
}
