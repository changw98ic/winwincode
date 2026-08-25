// SPDX-License-Identifier: Apache-2.0

#![allow(
    clippy::needless_pass_by_value,
    reason = "the runner consumes owned JSON and adapter errors at its test-only boundary"
)]

//! Versioned, test-only translation and execution of the frozen TypeScript
//! Delivery transcript through the canonical Rust Delivery seams.

use std::{
    collections::{BTreeMap, BTreeSet, HashMap, HashSet},
    fmt::Write as _,
    fs,
    path::{Path, PathBuf},
    str::FromStr,
    sync::{Arc, Mutex},
};

use rusqlite::{Connection, OpenFlags, OptionalExtension, params};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use winwincode_api::generated::{
    CommandCompletedResponse, CommandEnvelope, ControlPlaneWebSocketDeliveryChangedEvent,
    ControlPlaneWebSocketDeliveryChangedEventTypeValue, DeliveryDetailProjection, DeliveryGetQuery,
    DeliveryStageProjection, DeliveryStageSessionBindingProjection, ErrorEnvelope, ExecutionLimits,
    ExecutionOutcomeStatus, ExecutionWorkspace, ExecutionWorkspaceWriteMode, JobOutcomeMessage,
    QueryResultResponse, RepositoryScope, RuntimeProjectionGetQuery, SessionBindingMessage,
};
use winwincode_control_plane::{
    ControlPlane, ControlPlaneConfig, EventPublishError, EventPublisher, OutboxEvent,
    delivery_execution::{
        DeliveryExecutionConfig, DeliveryExecutionError, DeliveryExecutionPortError,
        ExecutionJobDispatcher, prepare_delivery_advance,
    },
    strongflow_projection::{
        DeliveryRuntimeReadRequest, ProductSessionRuntimeReadRequest,
        StrongFlowProjectionQueryPort, StrongFlowProjectionSources, TrustedProjectionReadError,
        TrustedPublicationProjectionAdapter, TrustedPublicationProjectionRead,
        TrustedRuntimeProjectionAdapter, TrustedRuntimeProjectionRead,
    },
    test_support::{
        DeliveryRepositoryFactsFixture, DeliverySpecFactsFixture, delivery_advance_command_facts,
        delivery_attention_command_facts, delivery_spec_command_facts,
    },
};
use winwincode_delivery::{
    application::{
        CoordinationError, CoordinationErrorCode,
        attention::{AttentionDecision, ResolveAttentionInput, resolve_attention},
        solution_review::test_support::{
            InvalidTaskProposalFixture, PreparedSolutionReviewFixture, SolutionDiagramFixture,
            SolutionFixture, SolutionReviewDecisionFixture, SolutionReviewFixture,
            invalid_task_proposals_fixture, prepare_solution_review_fixture,
            settle_solution_review_fixture,
        },
        stage::{
            AdvanceStageInput, NewStageIdentities, ReviewAttentionSeed, SessionBindingAuthority,
            StageAdvanceEffect, StageAdvanceResult, TerminalArtifactReference,
            TerminalOutcomeStatus, advance, advance_rework,
            test_support::{
                active_lease_identity, delivery_terminal_outcome_facts, session_binding_authority,
                terminal_outcome_metadata, terminal_worker_outcome, verify_terminal_outcome,
            },
        },
        verdict::{
            SubmitVerdictFacts,
            test_support::{VerdictFixtureOutcome, verdict_facts_fixture},
        },
    },
    domain::{
        AttentionItemType, Delivery, DeliveryStage, FrozenDeliveryCandidate, RepositoryRef,
        SessionBindingId, StageRunActorType, StageRunStatus,
        candidate::{
            CandidateHunkFact, CandidatePathFact, CandidatePathState,
            test_support::{CandidateFixtureInput, freeze_candidate_fixture},
        },
        rework::{
            ReworkAuthorization, ReworkDecision,
            test_support::{
                freeze_rework_replacement_candidate_fixture, precise_rework_authorization_fixture,
            },
        },
    },
    projection::runtime::{
        RuntimeProjection,
        test_support::{
            RuntimeAuthorityFixture, RuntimeFactFixture, accepted_binding, accepted_event,
        },
    },
    store::{
        DELIVERY_STORE_SCHEMA_VERSION, DeliveryJournalCodec, DeliveryMutationOperation,
        DeliveryStoreManifest, DeliveryStoreRecord,
    },
};
use winwincode_domain::{
    AttentionItemId, CodexThreadId, DeliveryId, ExecutionAckSequence, ExecutionJobId, FencingToken,
    Instant, LeaseId, ProductSessionId, RepositoryId, RequestId, Revision, Sha256Digest,
    StageRunId, WorkerId, WorkerInstanceId, WorkerSessionId,
};
use winwincode_storage::{
    AggregateJournalKey, AggregateJournalPublication, AggregateJournalRecord, NewOutboxEvent,
    ProductStateStorage, ProjectionEventStream, ReceiptActorKey, ReceiptIdentity, ReceiptScopeKey,
    SqliteStorage, StateCommit,
};

const PLAN_SCHEMA: &str = "winwincode.delivery-strongflow-differential-plan.v2";
const RESULT_SCHEMA: &str = "winwincode.delivery-strongflow-rust-differential-result.v1";
const API_SCHEMA: &str = "winwincode/v1";
const CROCKFORD_BASE32: &[u8; 32] = b"0123456789ABCDEFGHJKMNPQRSTVWXYZ";

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct DifferentialPlan {
    schema_version: String,
    oracle_schema_version: String,
    bindings: PlanBindings,
    scenarios: Vec<ScenarioPlan>,
}

impl DifferentialPlan {
    pub fn oracle_schema_version(&self) -> &str {
        &self.oracle_schema_version
    }

    pub fn scenarios(&self) -> &[ScenarioPlan] {
        &self.scenarios
    }
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct PlanBindings {
    #[serde(rename = "ORACLE_ROOT")]
    oracle_root: PathBuf,
    #[serde(rename = "NODE_EXECUTABLE")]
    node_executable: String,
    #[serde(rename = "AUTH_PROOF")]
    auth_proof: String,
    #[serde(rename = "fixtureRandomIdentities")]
    fixture_random_identities: BTreeMap<String, String>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ScenarioPlan {
    id: String,
    commands: Vec<PlanCommand>,
    #[serde(rename = "terminalOutcomeStatusBySourceCommandIndex")]
    terminal_outcome_status_by_source_command_index: BTreeMap<usize, ExecutionOutcomeStatus>,
}

impl ScenarioPlan {
    pub fn command_count(&self) -> usize {
        self.commands.len()
    }

    pub fn execution_port_message_count(&self) -> usize {
        let session_binding_messages = self
            .commands
            .iter()
            .enumerate()
            .filter(|(index, command)| {
                let PlanCommand::StrongFlowRequest { request } = command else {
                    return false;
                };
                if request.get("operation").and_then(Value::as_str) != Some("bindSession") {
                    return false;
                }
                let stage_run_id = request
                    .pointer("/payload/stageRunId")
                    .and_then(Value::as_str);
                !self.commands[..*index].iter().rev().any(|candidate| {
                    let PlanCommand::StrongFlowRequest { request } = candidate else {
                        return false;
                    };
                    request.get("operation").and_then(Value::as_str) == Some("startStage")
                        && request
                            .pointer("/payload/stageRunId")
                            .and_then(Value::as_str)
                            == stage_run_id
                        && request
                            .pointer("/payload/actorType")
                            .and_then(Value::as_str)
                            == Some("human")
                })
            })
            .count();
        session_binding_messages + self.terminal_outcome_status_by_source_command_index.len()
    }

    fn validate_terminal_outcome_statuses(&self) -> Result<(), String> {
        let mut seen = HashSet::new();
        let mut expected_source_indexes = BTreeSet::new();
        for (source_index, command) in self.commands.iter().enumerate() {
            let PlanCommand::StrongFlowRequest { request } = command else {
                continue;
            };
            if request.get("operation").and_then(Value::as_str) != Some("submitVerdict") {
                continue;
            }
            let fact = terminal_verifier_fact(object(request, "payload")?)?;
            if seen.insert(fact.key) {
                expected_source_indexes.insert(source_index);
            }
        }
        let actual_source_indexes = self
            .terminal_outcome_status_by_source_command_index
            .keys()
            .copied()
            .collect::<BTreeSet<_>>();
        if actual_source_indexes != expected_source_indexes {
            return Err(format!(
                "scenario {} terminal outcome status indexes differ: expected {:?}, got {:?}",
                self.id, expected_source_indexes, actual_source_indexes
            ));
        }
        if let Some((source_index, status)) = self
            .terminal_outcome_status_by_source_command_index
            .iter()
            .find(|(_, status)| {
                !matches!(
                    status,
                    ExecutionOutcomeStatus::Succeeded | ExecutionOutcomeStatus::InfrastructureError
                )
            })
        {
            return Err(format!(
                "scenario {} terminal outcome source {source_index} has unsupported status {status:?}",
                self.id
            ));
        }
        Ok(())
    }
}

#[derive(Debug, Deserialize)]
#[serde(tag = "kind", deny_unknown_fields)]
enum PlanCommand {
    #[serde(rename = "strongflow.request")]
    StrongFlowRequest { request: Value },
    #[serde(rename = "fixture.execution-source.replace")]
    ExecutionSourceReplace { input: Value },
    #[serde(rename = "fixture.service.restart")]
    ServiceRestart { input: Value },
    #[serde(rename = "fixture.store.seed-snapshot")]
    StoreSeedSnapshot { input: Value },
    #[serde(rename = "fixture.store.corrupt-record")]
    StoreCorruptRecord { input: Value },
    #[serde(rename = "fixture.store.restore-record")]
    StoreRestoreRecord { input: Value },
}

pub fn run_differential_plan(plan: &DifferentialPlan) -> Result<Value, String> {
    if plan.schema_version != PLAN_SCHEMA {
        return Err(format!(
            "differential plan schema must be {PLAN_SCHEMA}, got {}",
            plan.schema_version
        ));
    }
    if !plan.bindings.fixture_random_identities.is_empty() {
        return Err("fixtureRandomIdentities must remain empty in the frozen v2 plan".into());
    }
    if plan.bindings.auth_proof.trim().is_empty() || plan.bindings.node_executable.trim().is_empty()
    {
        return Err("differential bindings must hydrate AUTH_PROOF and NODE_EXECUTABLE".into());
    }
    reject_placeholders(plan)?;
    fs::create_dir_all(&plan.bindings.oracle_root).map_err(io_error)?;

    let mut scenarios = Vec::with_capacity(plan.scenarios.len());
    for scenario in &plan.scenarios {
        scenario.validate_terminal_outcome_statuses()?;
        let mut runner = ScenarioRunner::open(
            &plan.bindings.oracle_root,
            &scenario.id,
            planned_candidate_inputs(scenario)?,
            scenario
                .terminal_outcome_status_by_source_command_index
                .clone(),
        )?;
        let mut commands = Vec::new();
        for (source_index, command) in scenario.commands.iter().enumerate() {
            let executed = runner.execute(source_index, command).map_err(|error| {
                format!(
                    "scenario {} command {source_index} failed: {error}",
                    scenario.id
                )
            })?;
            commands.extend(executed);
        }
        runner.require_all_terminal_outcome_statuses_consumed()?;
        fold_human_binding_provenance(scenario, &mut commands)?;
        scenarios.push(json!({
            "id": scenario.id,
            "commands": commands,
            "observation": runner.observe()?,
        }));
    }

    Ok(json!({
        "schemaVersion": RESULT_SCHEMA,
        "oracleSchemaVersion": plan.oracle_schema_version,
        "scenarios": scenarios,
    }))
}

/// Builds the closed terminal-status provenance only for this Rust test
/// target's embedded-oracle fallback plan. The executor itself consumes the
/// plan map and never derives product authority from legacy runtime events.
pub fn local_fixture_terminal_outcome_statuses(source_scenario: &Value) -> Result<Value, String> {
    let commands = source_scenario
        .get("commands")
        .and_then(Value::as_array)
        .ok_or_else(|| "fixture source scenario commands must be an array".to_owned())?;
    let mut seen = HashSet::new();
    let mut statuses = serde_json::Map::new();
    for (source_index, command) in commands.iter().enumerate() {
        if command.get("kind").and_then(Value::as_str) != Some("strongflow.request") {
            continue;
        }
        let request = object(command, "request")?;
        if request.get("operation").and_then(Value::as_str) != Some("submitVerdict") {
            continue;
        }
        let payload = object(request, "payload")?;
        let fact = terminal_verifier_fact(payload)?;
        if !seen.insert(fact.key.clone()) {
            continue;
        }
        let status = fixture_terminal_outcome_status(source_scenario, &fact.key)?;
        statuses.insert(
            source_index.to_string(),
            serde_json::to_value(status).map_err(string_error)?,
        );
    }
    Ok(Value::Object(statuses))
}

fn fixture_terminal_outcome_status(
    source_scenario: &Value,
    fact_key: &TerminalVerifierFactKey,
) -> Result<ExecutionOutcomeStatus, String> {
    let commands = source_scenario
        .get("commands")
        .and_then(Value::as_array)
        .ok_or_else(|| "fixture source scenario commands must be an array".to_owned())?;
    let mut completed = Vec::new();
    for (source_index, command) in commands.iter().enumerate() {
        if command
            .pointer("/request/operation")
            .and_then(Value::as_str)
            != Some("submitVerdict")
            || command.pointer("/response/ok").and_then(Value::as_bool) != Some(true)
        {
            continue;
        }
        let payload = object(object(command, "request")?, "payload")?;
        if terminal_verifier_fact(payload)?.key == *fact_key {
            completed.push((source_index, command));
        }
    }
    let [(source_index, command)] = completed.as_slice() else {
        return Err(format!(
            "terminal verifier fact must have exactly one completed submit response, found {}",
            completed.len()
        ));
    };
    let delivery = command
        .pointer("/response/result/delivery")
        .ok_or_else(|| format!("completed submit source {source_index} lacks response delivery"))?;
    let stage_runs = delivery
        .get("stageRuns")
        .and_then(Value::as_array)
        .ok_or_else(|| {
            format!("completed submit source {source_index} stageRuns must be an array")
        })?;
    let verifier = stage_runs
        .last()
        .ok_or_else(|| format!("completed submit source {source_index} lacks a final StageRun"))?;
    if required_str(verifier, "role")? != "verifier" {
        return Err(format!(
            "completed submit source {source_index} final StageRun must be verifier"
        ));
    }
    let verifier_status = required_str(verifier, "status")?;
    let verdict_status = required_str(object(delivery, "verdict")?, "status")?;
    match (verifier_status, verdict_status) {
        ("succeeded", "fail" | "inconclusive" | "pass") => Ok(ExecutionOutcomeStatus::Succeeded),
        ("failed", "infra_error") => Ok(ExecutionOutcomeStatus::InfrastructureError),
        _ => Err(format!(
            "completed submit source {source_index} has unmigratable verifier/verdict status pair {verifier_status:?}/{verdict_status:?}"
        )),
    }
}

fn planned_candidate_inputs(scenario: &ScenarioPlan) -> Result<HashMap<String, Value>, String> {
    let mut candidates = HashMap::new();
    for command in &scenario.commands {
        let PlanCommand::StrongFlowRequest { request } = command else {
            continue;
        };
        if request.get("operation").and_then(Value::as_str) != Some("submitVerdict") {
            continue;
        }
        let candidate = object(object(request, "payload")?, "candidate")?;
        let producer = canonical_stage_run_id(required_str(candidate, "producerStageRunId")?);
        match candidates.entry(producer.0) {
            std::collections::hash_map::Entry::Vacant(entry) => {
                entry.insert(candidate.clone());
            }
            std::collections::hash_map::Entry::Occupied(entry) if entry.get() == candidate => {}
            std::collections::hash_map::Entry::Occupied(_) => {
                return Err(format!(
                    "scenario {} maps one producer StageRun to conflicting candidates",
                    scenario.id
                ));
            }
        }
    }
    Ok(candidates)
}

fn fold_human_binding_provenance(
    scenario: &ScenarioPlan,
    commands: &mut Vec<Value>,
) -> Result<(), String> {
    for (bind_index, command) in scenario.commands.iter().enumerate() {
        let PlanCommand::StrongFlowRequest { request } = command else {
            continue;
        };
        if required_str(request, "operation")? != "bindSession" {
            continue;
        }
        let stage_run_id = required_str(object(request, "payload")?, "stageRunId")?;
        let Some(start_index) = scenario.commands[..bind_index]
            .iter()
            .enumerate()
            .rev()
            .find_map(|(index, candidate)| {
                let PlanCommand::StrongFlowRequest { request } = candidate else {
                    return None;
                };
                (request.get("operation").and_then(Value::as_str) == Some("startStage")
                    && request
                        .pointer("/payload/stageRunId")
                        .and_then(Value::as_str)
                        == Some(stage_run_id)
                    && request
                        .pointer("/payload/actorType")
                        .and_then(Value::as_str)
                        == Some("human"))
                .then_some(index)
            })
        else {
            continue;
        };
        let binding_position = commands.iter().position(|entry| {
            entry["sourceCommandIndexes"] == json!([bind_index])
                && entry["kind"] == "execution-port.message"
        });
        let advance_position = commands.iter().position(|entry| {
            entry["sourceCommandIndexes"] == json!([start_index])
                && entry["request"].get("command").and_then(Value::as_str)
                    == Some("delivery.advance")
        });
        let Some(advance_position) = advance_position else {
            return Err(format!(
                "scenario {} lost Human binding provenance for source {bind_index}",
                scenario.id
            ));
        };
        commands[advance_position]["sourceCommandIndexes"]
            .as_array_mut()
            .expect("advance provenance")
            .push(json!(bind_index));
        if let Some(binding_position) = binding_position {
            commands.remove(binding_position);
        }
    }
    Ok(())
}

fn reject_placeholders(plan: &DifferentialPlan) -> Result<(), String> {
    let encoded = serde_json::to_string(&plan.scenarios).map_err(string_error)?;
    for placeholder in ["<ORACLE_ROOT>", "<NODE_EXECUTABLE>", "<AUTH_PROOF>"] {
        if encoded.contains(placeholder) {
            return Err(format!("execution plan contains unhydrated {placeholder}"));
        }
    }
    Ok(())
}

impl Serialize for ScenarioPlan {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        json!({
            "id": self.id,
            "commands": self.commands,
            "terminalOutcomeStatusBySourceCommandIndex":
                self.terminal_outcome_status_by_source_command_index,
        })
        .serialize(serializer)
    }
}

impl Serialize for PlanCommand {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        let value = match self {
            Self::StrongFlowRequest { request } => {
                json!({ "kind": "strongflow.request", "request": request })
            }
            Self::ExecutionSourceReplace { input } => {
                json!({ "kind": "fixture.execution-source.replace", "input": input })
            }
            Self::ServiceRestart { input } => {
                json!({ "kind": "fixture.service.restart", "input": input })
            }
            Self::StoreSeedSnapshot { input } => {
                json!({ "kind": "fixture.store.seed-snapshot", "input": input })
            }
            Self::StoreCorruptRecord { input } => {
                json!({ "kind": "fixture.store.corrupt-record", "input": input })
            }
            Self::StoreRestoreRecord { input } => {
                json!({ "kind": "fixture.store.restore-record", "input": input })
            }
        };
        value.serialize(serializer)
    }
}

struct ScenarioRunner {
    id: String,
    home: PathBuf,
    control_plane: Option<ControlPlane>,
    published: Arc<Mutex<Vec<OutboxEvent>>>,
    projection_authority: ProjectionAuthority,
    repository_scope: RepositoryScope,
    delivery_id: Option<DeliveryId>,
    legacy_spec_identity: Option<LegacySpecIdentity>,
    execution_source: ExecutionSource,
    revision_map: BTreeMap<u64, u64>,
    leases: HashMap<String, LeaseFixture>,
    candidates: HashMap<String, FrozenDeliveryCandidate>,
    planned_candidates: HashMap<String, Value>,
    terminal_outcome_statuses: BTreeMap<usize, ExecutionOutcomeStatus>,
    rework_authorization: Option<ReworkAuthorization>,
    terminal_facts: HashSet<TerminalVerifierFactKey>,
    originals: HashMap<u64, Vec<u8>>,
    clock: FixtureClock,
}

enum StagePreparationError {
    Coordination(CoordinationError),
    InvalidRequest(String),
    WrongState(String),
}

#[derive(Clone, Default)]
struct ExecutionSource {
    candidate: Option<Value>,
    candidate_fact: Option<FrozenDeliveryCandidate>,
    runtime_events: Vec<Value>,
}

#[derive(Clone, Default)]
struct ProjectionAuthority {
    runtime: Arc<Mutex<Option<TrustedRuntimeProjectionRead>>>,
    publication: Arc<Mutex<Option<TrustedPublicationProjectionRead>>>,
}

impl ProjectionAuthority {
    fn sources(&self) -> StrongFlowProjectionSources {
        StrongFlowProjectionSources::new(
            Box::new(RuntimeSourceAdapter {
                read: Arc::clone(&self.runtime),
            }),
            Box::new(PublicationSourceAdapter {
                read: Arc::clone(&self.publication),
            }),
        )
    }

    fn replace(
        &self,
        scope: &RepositoryScope,
        delivery: &Delivery,
        source: &ExecutionSource,
    ) -> Result<(), String> {
        let projection = semantic_runtime_projection(delivery, &source.runtime_events)?;
        let accepted_sequence = projection
            .snapshot()
            .sessions
            .iter()
            .map(|session| session.as_of_sequence)
            .max()
            .unwrap_or(0);
        let runtime = TrustedRuntimeProjectionRead::try_new(
            scope.clone(),
            delivery.revision(),
            Revision(i64::try_from(accepted_sequence).map_err(string_error)?),
            accepted_sequence,
            Instant(millis_to_rfc3339(delivery.snapshot().updated_at_millis)?),
            projection,
            Sha256Digest(format!("sha256:{}", "a".repeat(64))),
        )
        .map_err(string_error)?;
        let publication = TrustedPublicationProjectionRead::try_new(
            scope.clone(),
            delivery.id().clone(),
            delivery.revision(),
            Revision(0),
            source.candidate_fact.clone(),
            None,
            Sha256Digest(format!("sha256:{}", "b".repeat(64))),
        )
        .map_err(string_error)?;
        *self
            .runtime
            .lock()
            .map_err(|_| "runtime projection fixture lock poisoned".to_owned())? = Some(runtime);
        *self
            .publication
            .lock()
            .map_err(|_| "publication projection fixture lock poisoned".to_owned())? =
            Some(publication);
        Ok(())
    }
}

struct RuntimeSourceAdapter {
    read: Arc<Mutex<Option<TrustedRuntimeProjectionRead>>>,
}

impl TrustedRuntimeProjectionAdapter for RuntimeSourceAdapter {
    fn read_delivery(
        &self,
        request: &DeliveryRuntimeReadRequest,
    ) -> Result<TrustedRuntimeProjectionRead, TrustedProjectionReadError> {
        let read = self
            .read
            .lock()
            .map_err(|_| TrustedProjectionReadError::TemporarilyUnavailable)?;
        let read = read
            .as_ref()
            .ok_or(TrustedProjectionReadError::Unavailable)?;
        if read.scope() != request.scope()
            || read.snapshot().delivery_id != *request.delivery_id()
            || read.delivery_revision() != request.delivery_revision()
            || request.expected().is_some_and(|expected| {
                expected.ledger_revision() != read.ledger_revision()
                    || expected.accepted_sequence() != read.accepted_sequence()
            })
        {
            return Err(TrustedProjectionReadError::Stale);
        }
        Ok(read.clone())
    }

    fn read_product_session(
        &self,
        request: &ProductSessionRuntimeReadRequest,
    ) -> Result<TrustedRuntimeProjectionRead, TrustedProjectionReadError> {
        let read = self
            .read
            .lock()
            .map_err(|_| TrustedProjectionReadError::TemporarilyUnavailable)?;
        let read = read
            .as_ref()
            .ok_or(TrustedProjectionReadError::Unavailable)?;
        if read.scope() != request.scope()
            || !read
                .snapshot()
                .sessions
                .iter()
                .any(|session| &session.product_session_id == request.product_session_id())
            || request.expected().is_some_and(|expected| {
                expected.ledger_revision() != read.ledger_revision()
                    || expected.accepted_sequence() != read.accepted_sequence()
            })
        {
            return Err(TrustedProjectionReadError::Stale);
        }
        Ok(read.clone())
    }
}

struct PublicationSourceAdapter {
    read: Arc<Mutex<Option<TrustedPublicationProjectionRead>>>,
}

impl TrustedPublicationProjectionAdapter for PublicationSourceAdapter {
    fn read_current(
        &self,
        scope: &RepositoryScope,
        delivery_id: &DeliveryId,
        delivery_revision: u64,
        expected_publication_revision: Option<&Revision>,
    ) -> Result<TrustedPublicationProjectionRead, TrustedProjectionReadError> {
        let read = self
            .read
            .lock()
            .map_err(|_| TrustedProjectionReadError::TemporarilyUnavailable)?;
        let read = read
            .as_ref()
            .ok_or(TrustedProjectionReadError::Unavailable)?;
        if read.scope() != scope
            || read.delivery_id() != delivery_id
            || read.delivery_revision() != delivery_revision
            || expected_publication_revision
                .is_some_and(|revision| revision != read.publication_revision())
        {
            return Err(TrustedProjectionReadError::Stale);
        }
        Ok(read.clone())
    }
}

fn semantic_runtime_projection(
    delivery: &Delivery,
    raw_events: &[Value],
) -> Result<RuntimeProjection, String> {
    let mut bindings = Vec::new();
    let mut accepted_events = Vec::new();
    for binding in &delivery.snapshot().session_bindings {
        if binding.worker_session_id.is_none() || binding.codex_thread_id.is_none() {
            continue;
        }
        let run = delivery
            .snapshot()
            .stage_runs
            .iter()
            .find(|run| run.id == binding.stage_run_id)
            .ok_or_else(|| "runtime fixture binding lost its StageRun".to_owned())?;
        let observed_count = raw_events
            .iter()
            .filter(|event| {
                event
                    .pointer("/source/sessionId")
                    .and_then(Value::as_str)
                    .is_some_and(|session| {
                        binding
                            .worker_session_id
                            .as_ref()
                            .is_some_and(|worker| canonical_id("wsn_", session) == worker.0)
                    })
            })
            .count();
        let terminal = matches!(
            run.status,
            StageRunStatus::Succeeded | StageRunStatus::Failed | StageRunStatus::Cancelled
        );
        let event_count = if terminal {
            observed_count.max(1)
        } else {
            observed_count
        };
        let authority = RuntimeAuthorityFixture {
            lease_id: LeaseId(canonical_id("lse_", &run.id.0)),
            fencing_token: FencingToken(run.attempt.to_string()),
            worker_id: WorkerId(canonical_id("wrk_", &run.id.0)),
            worker_instance_id: WorkerInstanceId(canonical_id("wki_", &run.id.0)),
        };
        let accepted = accepted_binding(
            delivery,
            &binding.id,
            authority,
            terminal.then_some(u64::try_from(event_count).map_err(string_error)?),
        )
        .map_err(string_error)?;
        for sequence in 1..=event_count {
            accepted_events.push(
                accepted_event(
                    &accepted,
                    u64::try_from(sequence).map_err(string_error)?,
                    &format!("runtime:{}:{sequence}", binding.id.0),
                    RuntimeFactFixture::Checkpoint,
                )
                .map_err(string_error)?,
            );
        }
        bindings.push(accepted);
    }
    RuntimeProjection::replay(delivery, bindings, &accepted_events).map_err(string_error)
}

#[derive(Clone)]
struct LeaseFixture {
    authority: SessionBindingAuthority,
    binding_message: SessionBindingMessage,
    legacy_session_id: String,
}

#[derive(Clone, Debug, Eq, Hash, PartialEq)]
struct TerminalVerifierFactKey {
    legacy_session_id: String,
    last_event_sequence: u64,
    occurred_at_millis: u64,
}

struct TerminalVerifierFact {
    key: TerminalVerifierFactKey,
    summary: String,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum LegacyVerificationVerdict {
    Pass,
    Fail,
    Inconclusive,
    InfraError,
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct LegacyVerificationIdentity {
    candidate_ref: String,
    delivery_spec_id: String,
    delivery_spec_revision: u64,
    criterion_ids: BTreeSet<String>,
}

struct LegacyVerificationResult {
    role: String,
    verdict: LegacyVerificationVerdict,
    identity: LegacyVerificationIdentity,
}

#[derive(Clone, Debug)]
struct VerificationSemanticAuthority {
    identity: LegacyVerificationIdentity,
    required_roles: BTreeSet<String>,
}

#[derive(Clone, Debug)]
struct LegacySpecIdentity {
    id: String,
    revision: u64,
}

struct CapturingPublisher {
    events: Arc<Mutex<Vec<OutboxEvent>>>,
}

impl EventPublisher for CapturingPublisher {
    fn publish(&mut self, event: &OutboxEvent) -> Result<(), EventPublishError> {
        self.events
            .lock()
            .map_err(|_| EventPublishError::new("differential publisher lock poisoned"))?
            .push(event.clone());
        Ok(())
    }
}

#[derive(Default)]
struct RecordingDispatcher {
    jobs: Vec<winwincode_api::generated::ExecutionJob>,
}

impl ExecutionJobDispatcher for RecordingDispatcher {
    fn dispatch(
        &mut self,
        job: &winwincode_api::generated::ExecutionJob,
    ) -> Result<(), DeliveryExecutionPortError> {
        self.jobs.push(job.clone());
        Ok(())
    }
}

impl ScenarioRunner {
    fn open(
        root: &Path,
        id: &str,
        planned_candidates: HashMap<String, Value>,
        terminal_outcome_statuses: BTreeMap<usize, ExecutionOutcomeStatus>,
    ) -> Result<Self, String> {
        safe_component(id)?;
        let home = root.join(id).join("rust-delivery-store");
        if home.exists() {
            fs::remove_dir_all(&home).map_err(io_error)?;
        }
        fs::create_dir_all(&home).map_err(io_error)?;
        let published = Arc::new(Mutex::new(Vec::new()));
        let projection_authority = ProjectionAuthority::default();
        let mut control_plane = ControlPlane::start_local(
            ControlPlaneConfig::local(&home),
            Box::new(CapturingPublisher {
                events: Arc::clone(&published),
            }),
        )
        .map_err(string_error)?;
        control_plane
            .install_strongflow_projection_sources(projection_authority.sources())
            .map_err(string_error)?;
        let repository_scope = serde_json::from_value(fixture_scope(id)).map_err(string_error)?;
        Ok(Self {
            id: id.to_owned(),
            home,
            control_plane: Some(control_plane),
            published,
            projection_authority,
            repository_scope,
            delivery_id: None,
            legacy_spec_identity: None,
            execution_source: ExecutionSource::default(),
            revision_map: BTreeMap::new(),
            leases: HashMap::new(),
            candidates: HashMap::new(),
            planned_candidates,
            terminal_outcome_statuses,
            rework_authorization: None,
            terminal_facts: HashSet::new(),
            originals: HashMap::new(),
            clock: FixtureClock::default(),
        })
    }

    #[allow(
        clippy::too_many_lines,
        reason = "the closed fixture command union keeps all boundary effects visible in one dispatcher"
    )]
    fn execute(
        &mut self,
        source_index: usize,
        command: &PlanCommand,
    ) -> Result<Vec<Value>, String> {
        match command {
            PlanCommand::StrongFlowRequest { request } => {
                self.execute_request(source_index, request)
            }
            PlanCommand::ExecutionSourceReplace { input } => {
                self.execution_source =
                    parse_execution_source_with_candidates(input, &self.candidates);
                self.refresh_projection_authority()?;
                Ok(vec![fixture_entry(
                    source_index,
                    "fixture.runtime-source.replace",
                    json!({
                        "candidate": self.execution_source.candidate,
                        "runtimeEvents": self.execution_source.runtime_events,
                    }),
                    fixture_completed(json!({
                        "candidatePresent": self.execution_source.candidate.is_some(),
                        "runtimeEventCount": self.execution_source.runtime_events.len(),
                    })),
                )])
            }
            PlanCommand::ServiceRestart { input } => {
                self.restart_control_plane()?;
                Ok(vec![fixture_entry(
                    source_index,
                    "fixture.service.restart",
                    input.clone(),
                    fixture_completed(json!({ "durableStoreReopened": true })),
                )])
            }
            PlanCommand::StoreSeedSnapshot { input } => {
                let snapshot = input
                    .get("snapshot")
                    .cloned()
                    .ok_or_else(|| "seed-snapshot input lacks snapshot".to_owned())?;
                let canonical = migrate_legacy_snapshot(snapshot)?;
                let delivery =
                    Delivery::decode_json(&serde_json::to_vec(&canonical).map_err(string_error)?)
                        .map_err(string_error)?;
                self.stop_control_plane()?;
                seed_snapshot_sqlite(&self.home, &self.id, &delivery)?;
                self.start_control_plane()?;
                self.delivery_id = Some(delivery.id().clone());
                self.revision_map
                    .insert(delivery.revision(), delivery.revision());
                self.refresh_projection_authority()?;
                let response = fixture_completed(json!({
                    "deliveryId": delivery.id(),
                    "currentRevision": delivery.revision(),
                }));
                Ok(vec![fixture_entry(
                    source_index,
                    "fixture.store.seed-snapshot",
                    json!({ "snapshot": canonical }),
                    response,
                )])
            }
            PlanCommand::StoreCorruptRecord { input } => {
                let sequence = sequence(input)?;
                let delivery_id = self
                    .delivery_id
                    .as_ref()
                    .ok_or_else(|| "corruption fixture requires a Delivery".to_owned())?
                    .clone();
                self.stop_control_plane()?;
                let original = sqlite_journal_record_payload(&self.home, &delivery_id, sequence)?;
                self.originals.insert(sequence, original.clone());
                let mut record: Value = serde_json::from_slice(&original).map_err(string_error)?;
                let mutation = required_str(input, "mutation")?;
                match mutation {
                    "snapshot.status=ready-without-digest-update" => {
                        record["snapshot"]["status"] = Value::String("ready".into());
                    }
                    other => return Err(format!("unsupported controlled corruption {other}")),
                }
                sqlite_replace_journal_record_payload(
                    &self.home,
                    &delivery_id,
                    sequence,
                    &serde_json::to_vec(&record).map_err(string_error)?,
                )?;
                self.start_control_plane()?;
                Ok(vec![fixture_entry(
                    source_index,
                    "fixture.store.corrupt-record",
                    input.clone(),
                    fixture_completed(json!({ "sequence": sequence })),
                )])
            }
            PlanCommand::StoreRestoreRecord { input } => {
                let sequence = sequence(input)?;
                let delivery_id = self
                    .delivery_id
                    .as_ref()
                    .ok_or_else(|| "restore fixture requires a Delivery".to_owned())?
                    .clone();
                let original = self
                    .originals
                    .get(&sequence)
                    .ok_or_else(|| format!("record {sequence} has no saved original"))?
                    .clone();
                self.stop_control_plane()?;
                sqlite_replace_journal_record_payload(
                    &self.home,
                    &delivery_id,
                    sequence,
                    &original,
                )?;
                self.start_control_plane()?;
                self.refresh_projection_authority()?;
                Ok(vec![fixture_entry(
                    source_index,
                    "fixture.store.restore-record",
                    input.clone(),
                    fixture_completed(json!({ "sequence": sequence })),
                )])
            }
        }
    }

    fn execute_request(
        &mut self,
        source_index: usize,
        request: &Value,
    ) -> Result<Vec<Value>, String> {
        let operation = LegacyOperation::from_str(required_str(request, "operation")?)?;
        match operation {
            LegacyOperation::Create => Ok(vec![self.create(source_index, request)?]),
            LegacyOperation::UpdateSpec => Ok(vec![self.update_spec(source_index, request)?]),
            LegacyOperation::StartStage => Ok(vec![self.start_stage(source_index, request)?]),
            LegacyOperation::BindSession => self.bind_session(source_index, request),
            LegacyOperation::ResolveAttention => self.resolve_attention(source_index, request),
            LegacyOperation::SubmitVerdict => self.submit_verdict(source_index, request),
            LegacyOperation::GetProjection => self.get_projection(source_index, request),
        }
    }

    fn create(&mut self, source_index: usize, legacy: &Value) -> Result<Value, String> {
        let payload = object(legacy, "payload")?;
        if self.delivery_id.is_some()
            && let Some(tasks) = payload.get("tasks").and_then(Value::as_array)
            && !tasks.is_empty()
        {
            if legacy_task_graph_has_cycle(tasks)? {
                return self.reject_task_dag_cycle(source_index, legacy);
            }
            return Err(
                "legacy create tasks are non-canonical but do not form the frozen invalid cycle"
                    .to_owned(),
            );
        }
        let legacy_spec = payload
            .get("spec")
            .ok_or_else(|| "createDelivery payload lacks spec".to_owned())?;
        let delivery_id = DeliveryId(required_str(legacy_spec, "deliveryId")?.to_owned());
        let canonical_payload = canonical_create_payload(
            payload,
            &delivery_id,
            &self.repository_scope.repository_id.0,
        )?;
        let request = command_envelope(
            &self.id,
            "delivery.create",
            required_str(legacy, "requestId")?,
            0,
            canonical_payload,
        );
        let typed = strict_command_envelope(&request)?;
        let now = self
            .clock
            .create_time(required_u64(legacy_spec, "createdAtMillis")?);
        let facts = delivery_spec_command_facts(&typed, self.spec_facts_fixture(legacy_spec, now)?)
            .map_err(string_error)?;
        let result = self
            .control_plane_mut()
            .commit_delivery_command(&typed, &facts);
        let response = match result {
            Ok(_receipt) => {
                self.delivery_id = Some(delivery_id.clone());
                self.legacy_spec_identity = Some(LegacySpecIdentity {
                    id: required_str(legacy_spec, "id")?.to_owned(),
                    revision: required_u64(legacy_spec, "revision")?,
                });
                let delivery = self.query(&delivery_id)?;
                self.revision_map.insert(1, delivery.revision());
                self.refresh_projection_authority()?;
                completed_response(
                    "delivery.create",
                    required_str(legacy, "requestId")?,
                    0,
                    &delivery,
                    &self.id,
                )?
            }
            Err(error) => delivery_command_error_response(
                required_str(legacy, "requestId")?,
                &error,
                self.current_revision(&delivery_id),
            )?,
        };
        Ok(command_entry(
            source_index,
            "control-plane.command",
            request,
            response,
        ))
    }

    fn reject_task_dag_cycle(
        &mut self,
        source_index: usize,
        legacy: &Value,
    ) -> Result<Value, String> {
        let payload = object(legacy, "payload")?;
        let spec = payload
            .get("spec")
            .ok_or_else(|| "cyclic create migration lacks spec".to_owned())?;
        let (delivery, input) = cycle_validation_delivery(spec)?;
        let invalid_review = invalid_cycle_review_fixture(&delivery, &self.id)?;
        let primary_delivery_id = self
            .delivery_id
            .as_ref()
            .ok_or_else(|| "cycle validation requires the primary seeded Delivery".to_owned())?;
        let before = sqlite_durable_observation(&self.home, primary_delivery_id)?;
        let rejected = prepare_solution_review_fixture(&delivery, input, invalid_review)
            .expect_err("sealed cyclic proposal migration must fail closed");
        let after = sqlite_durable_observation(&self.home, primary_delivery_id)?;
        if before != after {
            return Err(
                "rejected cyclic solution review changed state, journal, receipt, or outbox"
                    .to_owned(),
            );
        }
        if rejected.message().is_empty() {
            return Err("sealed cyclic proposal rejection lost its reason".to_owned());
        }
        Ok(fixture_entry(
            source_index,
            "fixture.solution-review.validate",
            json!({
                "spec": delivery.snapshot().spec,
                "invalidProposalKind": "dependency-cycle",
            }),
            json!({
                "outcome": "rejected",
                "error": {
                    "code": "INVALID_REQUEST",
                    "message": rejected.message(),
                    "retryable": false,
                    "details": {},
                }
            }),
        ))
    }

    fn update_spec(&mut self, source_index: usize, legacy: &Value) -> Result<Value, String> {
        let payload = object(legacy, "payload")?;
        let delivery_id = DeliveryId(required_str(payload, "deliveryId")?.to_owned());
        let legacy_expected = required_u64(payload, "expectedRevision")?;
        let expected = self.actual_revision(legacy_expected);
        let canonical_payload = canonical_update_payload(
            payload,
            &delivery_id,
            &self.repository_scope.repository_id.0,
        )?;
        let request_id = required_str(legacy, "requestId")?;
        let request = command_envelope(
            &self.id,
            "delivery.update_spec",
            request_id,
            expected,
            canonical_payload,
        );
        let typed = strict_command_envelope(&request)?;
        let legacy_spec = payload
            .get("spec")
            .ok_or_else(|| "update spec missing spec".to_owned())?;
        let now = self
            .clock
            .create_time(required_u64(legacy_spec, "createdAtMillis")?);
        let facts = delivery_spec_command_facts(&typed, self.spec_facts_fixture(legacy_spec, now)?)
            .map_err(string_error)?;
        let result = self
            .control_plane_mut()
            .commit_delivery_command(&typed, &facts);
        let response = match result {
            Ok(_receipt) => {
                self.legacy_spec_identity = Some(LegacySpecIdentity {
                    id: required_str(legacy_spec, "id")?.to_owned(),
                    revision: required_u64(legacy_spec, "revision")?,
                });
                let delivery = self.query(&delivery_id)?;
                self.revision_map
                    .insert(legacy_expected.saturating_add(1), delivery.revision());
                self.refresh_projection_authority()?;
                completed_response(
                    "delivery.update_spec",
                    request_id,
                    expected,
                    &delivery,
                    &self.id,
                )?
            }
            Err(error) => delivery_command_error_response(
                request_id,
                &error,
                self.current_revision(&delivery_id),
            )?,
        };
        Ok(command_entry(
            source_index,
            "control-plane.command",
            request,
            response,
        ))
    }

    #[allow(
        clippy::too_many_lines,
        reason = "the translator keeps each typed advance branch and its real transaction adjacent"
    )]
    fn start_stage(&mut self, source_index: usize, legacy: &Value) -> Result<Value, String> {
        let payload = object(legacy, "payload")?;
        let delivery_id = DeliveryId(required_str(payload, "deliveryId")?.to_owned());
        let legacy_expected = required_u64(payload, "expectedRevision")?;
        let expected = self.actual_revision(legacy_expected);
        let request_id = required_str(legacy, "requestId")?;
        let request = command_envelope(
            &self.id,
            "delivery.advance",
            request_id,
            expected,
            json!({ "deliveryId": delivery_id }),
        );
        let typed = strict_command_envelope(&request)?;
        let transition = self.query(&delivery_id).and_then(|delivery| {
            let (outcome, lease) = self.terminal_handoff(&delivery)?;
            let source_stage_run_id = required_str(payload, "stageRunId")?;
            let stage_run_id = canonical_stage_run_id(source_stage_run_id);
            let now_millis = self
                .clock
                .mutation_time(delivery.snapshot().updated_at_millis);
            let review = payload
                .get("attention")
                .filter(|value| !value.is_null())
                .map(|attention| {
                    Ok::<_, String>(ReviewAttentionSeed {
                        title: required_str(attention, "title")?.to_owned(),
                        context: required_str(attention, "context")?.to_owned(),
                        assigned_to: canonical_id("sys_", &self.id),
                    })
                })
                .transpose()?;
            let input = AdvanceStageInput {
                expected_revision: expected,
                product_session_id: ProductSessionId(canonical_id("psn_", &stage_run_id.0)),
                identities: NewStageIdentities {
                    stage_run_id: stage_run_id.clone(),
                    execution_job_id: ExecutionJobId(canonical_id("job_", &stage_run_id.0)),
                    session_binding_id: canonical_session_binding_id(source_stage_run_id)?,
                    attention_item_id: payload
                        .get("attention")
                        .and_then(|attention| attention.get("id"))
                        .and_then(Value::as_str)
                        .map_or_else(
                            || AttentionItemId(canonical_id("att_", &stage_run_id.0)),
                            canonical_attention_item_id,
                        ),
                },
                review,
                previous_outcome: outcome,
                current_lease: lease,
                rework_authorization: None,
                now_millis,
            };
            let requested_stage = required_str(payload, "stage")?;
            let (mut transition, rework_authorization) = if requested_stage == "plan-review" {
                let attention = payload
                    .get("attention")
                    .filter(|value| !value.is_null())
                    .ok_or_else(|| "plan-review advance lacks its semantic fixture".to_owned())?;
                let transition = prepare_solution_review_fixture(
                    &delivery,
                    AdvanceStageInput {
                        review: None,
                        ..input
                    },
                    {
                        let mut fixture = solution_review_fixture(attention)?;
                        fixture.assigned_to = canonical_id("sys_", &self.id);
                        fixture
                    },
                )
                .map(PreparedSolutionReviewFixture::into_transition)
                .map_err(|error| StagePreparationError::InvalidRequest(error.to_string()));
                (transition, None)
            } else if requested_stage == "reworking" {
                let failed_candidate = self.current_verdict_candidate(&delivery)?;
                let authorization =
                    precise_rework_authorization_fixture(&delivery, &failed_candidate);
                let transition = advance_rework(
                    &delivery,
                    input,
                    ReworkDecision::Start(Box::new(authorization.clone())),
                )
                .map_err(StagePreparationError::Coordination);
                (transition, Some(authorization))
            } else {
                (
                    advance(&delivery, input).map_err(StagePreparationError::Coordination),
                    None,
                )
            };
            if let Ok(prepared) = &transition
                && let Err(error) = require_requested_stage(payload, prepared)
            {
                transition = Err(error);
            }
            Ok((transition, rework_authorization))
        });
        let response = match transition {
            Ok((Ok(transition), rework_authorization)) => match &transition.effect {
                StageAdvanceEffect::Dispatch(_) => {
                    let execution_config = self.execution_config(&delivery_id, &transition)?;
                    let pending = prepare_delivery_advance(
                        typed.request_id.clone(),
                        transition,
                        execution_config,
                    )
                    .map_err(string_error)?;
                    let mut dispatcher = RecordingDispatcher::default();
                    match self.control_plane_mut().commit_delivery_execution(
                        &typed,
                        &pending,
                        &mut dispatcher,
                    ) {
                        Ok(_receipt) => {
                            if let Some(authorization) = rework_authorization {
                                self.rework_authorization = Some(authorization);
                            }
                            let delivery = self.query(&delivery_id)?;
                            self.revision_map
                                .insert(legacy_expected.saturating_add(1), delivery.revision());
                            self.refresh_projection_authority()?;
                            completed_response(
                                "delivery.advance",
                                request_id,
                                expected,
                                &delivery,
                                &self.id,
                            )?
                        }
                        Err(error) => delivery_execution_error_response(
                            request_id,
                            &error,
                            self.current_revision(&delivery_id),
                        )?,
                    }
                }
                StageAdvanceEffect::Review(_) => {
                    let facts = delivery_advance_command_facts(
                        &typed,
                        self.repository_facts_fixture()?,
                        transition,
                    )
                    .map_err(string_error)?;
                    match self
                        .control_plane_mut()
                        .commit_delivery_command(&typed, &facts)
                    {
                        Ok(_receipt) => {
                            let delivery = self.query(&delivery_id)?;
                            self.revision_map
                                .insert(legacy_expected.saturating_add(1), delivery.revision());
                            self.refresh_projection_authority()?;
                            completed_response(
                                "delivery.advance",
                                request_id,
                                expected,
                                &delivery,
                                &self.id,
                            )?
                        }
                        Err(error) => delivery_command_error_response(
                            request_id,
                            &error,
                            self.current_revision(&delivery_id),
                        )?,
                    }
                }
                StageAdvanceEffect::Resume(_) | StageAdvanceEffect::Clarify(_) => {
                    canonical_error_envelope(
                        request_id,
                        "INVALID_REQUEST",
                        "legacy stage migration cannot issue an unsealed resume effect",
                        false,
                        BTreeMap::new(),
                    )?
                }
            },
            Ok((Err(StagePreparationError::Coordination(error)), _)) => {
                coordination_error_response(
                    request_id,
                    &error,
                    self.current_revision(&delivery_id),
                )?
            }
            Ok((Err(StagePreparationError::InvalidRequest(message)), _)) => {
                canonical_error_envelope(
                    request_id,
                    "INVALID_REQUEST",
                    &message,
                    false,
                    revision_details(self.current_revision(&delivery_id)),
                )?
            }
            Ok((Err(StagePreparationError::WrongState(message)), _)) => canonical_error_envelope(
                request_id,
                "WRONG_STATE",
                &message,
                false,
                revision_details(self.current_revision(&delivery_id)),
            )?,
            Err(error) => return Err(error),
        };
        Ok(command_entry(
            source_index,
            "control-plane.command",
            request,
            response,
        ))
    }

    #[allow(
        clippy::too_many_lines,
        reason = "the sealed session handshake and its two journal mutations stay visibly ordered"
    )]
    fn bind_session(&mut self, source_index: usize, legacy: &Value) -> Result<Vec<Value>, String> {
        let payload = object(legacy, "payload")?;
        let delivery_id = DeliveryId(required_str(payload, "deliveryId")?.to_owned());
        let stage_run_id = canonical_stage_run_id(required_str(payload, "stageRunId")?);
        let legacy_expected = required_u64(payload, "expectedRevision")?;
        let request_id = required_str(legacy, "requestId")?;
        let delivery = self.query(&delivery_id);
        if delivery.as_ref().is_ok_and(|delivery| {
            delivery
                .snapshot()
                .stage_runs
                .iter()
                .any(|run| run.id == stage_run_id && run.actor_type == StageRunActorType::Human)
        }) {
            self.revision_map.insert(
                legacy_expected.saturating_add(1),
                delivery
                    .as_ref()
                    .expect("Human stage was confirmed from this Delivery")
                    .revision(),
            );
            return Ok(Vec::new());
        }
        let dsh = required_str(payload, "dshSessionId")?;
        let codex = payload.get("codexSessionId").and_then(Value::as_str);
        let worker_session_id = WorkerSessionId(canonical_id("wsn_", dsh));
        let codex_thread_id = CodexThreadId(canonical_id(
            "cdx_",
            codex.unwrap_or("missing-codex-thread"),
        ));
        let authority = delivery.as_ref().ok().and_then(|delivery| {
            let run = delivery
                .snapshot()
                .stage_runs
                .iter()
                .find(|run| run.id == stage_run_id)?;
            let binding = delivery
                .snapshot()
                .session_bindings
                .iter()
                .find(|binding| binding.stage_run_id == stage_run_id)?;
            Some((
                binding.product_session_id.clone(),
                binding.execution_job_id.clone(),
                run.attempt,
            ))
        });
        let (product_session_id, execution_job_id, attempt) = authority.unwrap_or_else(|| {
            (
                ProductSessionId(canonical_id("psn_", &stage_run_id.0)),
                ExecutionJobId(canonical_id("job_", &stage_run_id.0)),
                1,
            )
        });
        let bound_at = self.clock.peek_next();
        let request = json!({
            "schemaVersion": API_SCHEMA,
            "messageId": canonical_id("xmsg_", request_id),
            "kind": "session.binding",
            "sentAt": millis_to_rfc3339(bound_at)?,
            "lease": {
                "attempt": attempt,
                "expiresAt": millis_to_rfc3339(bound_at.saturating_add(60_000))?,
                "fencingToken": attempt.to_string(),
                "issuedAt": millis_to_rfc3339(bound_at)?,
                "jobId": execution_job_id,
                "leaseId": canonical_id("lse_", &stage_run_id.0),
                "workerId": canonical_id("wrk_", &stage_run_id.0),
                "workerInstanceId": canonical_id("wki_", &stage_run_id.0),
            },
            "productSessionId": product_session_id,
            "workerSessionId": worker_session_id,
            "codexThreadId": codex_thread_id,
            "boundAt": millis_to_rfc3339(bound_at)?,
        });
        let message: SessionBindingMessage = serde_json::from_value(request.clone())
            .map_err(|error| format!("canonical SessionBindingMessage rejected: {error}"))?;
        if serde_json::to_value(&message).map_err(string_error)? != request {
            return Err("SessionBindingMessage did not round-trip exactly".to_owned());
        }
        let active_lease = active_lease_identity(
            message.lease.job_id.clone(),
            u64::try_from(message.lease.attempt).map_err(string_error)?,
            message.lease.lease_id.clone(),
            message.lease.fencing_token.clone(),
            message.lease.worker_id.clone(),
            message.lease.worker_instance_id.clone(),
            message.worker_session_id.clone(),
        );
        let binding_authority = session_binding_authority(
            active_lease,
            message.lease.issued_at.clone(),
            message.lease.expires_at.clone(),
        );
        let result = self
            .control_plane_mut()
            .commit_delivery_session_binding(&message, &binding_authority);
        let response = match result {
            Ok(commit) => {
                let worker = commit.worker_session_receipt();
                let thread = commit.codex_thread_receipt();
                if worker.revision.saturating_add(1) != thread.revision {
                    return Err("SessionBinding receipts are not consecutive".to_owned());
                }
                self.revision_map
                    .insert(legacy_expected.saturating_add(1), thread.revision);
                self.leases.insert(
                    stage_run_id.0.clone(),
                    LeaseFixture {
                        authority: binding_authority,
                        binding_message: message.clone(),
                        legacy_session_id: dsh.to_owned(),
                    },
                );
                self.refresh_projection_authority()?;
                json!({
                    "messageId": message.message_id,
                    "outcome": "completed",
                    "previousRevision": worker.revision.saturating_sub(1),
                    "currentRevision": thread.revision,
                    "commits": [
                        {
                            "operation": "accept_worker_session",
                            "previousRevision": worker.revision.saturating_sub(1),
                            "currentRevision": worker.revision,
                            "receipt": receipt_json(worker)?,
                        },
                        {
                            "operation": "report_codex_thread",
                            "previousRevision": worker.revision,
                            "currentRevision": thread.revision,
                            "receipt": receipt_json(thread)?,
                        }
                    ],
                })
            }
            Err(error) => session_binding_error_response(
                &message.message_id.0,
                &error,
                self.current_revision(&delivery_id),
            )?,
        };
        Ok(vec![command_entry(
            source_index,
            "execution-port.message",
            request,
            response,
        )])
    }

    #[allow(
        clippy::too_many_lines,
        reason = "resolution and the immediately authorized task promotion stay one translation step"
    )]
    fn resolve_attention(
        &mut self,
        source_index: usize,
        legacy: &Value,
    ) -> Result<Vec<Value>, String> {
        let payload = object(legacy, "payload")?;
        let delivery_id = DeliveryId(required_str(payload, "deliveryId")?.to_owned());
        let legacy_expected = required_u64(payload, "expectedRevision")?;
        let expected = self.actual_revision(legacy_expected);
        let request_id = required_str(legacy, "requestId")?;
        let decision = match required_str(payload, "status")? {
            "resolved" => "resolve",
            "dismissed" => "dismiss",
            other => return Err(format!("unknown Attention status {other}")),
        };
        let source = self.query(&delivery_id);
        let requested_attention_id =
            canonical_attention_item_id(required_str(payload, "attentionItemId")?);
        let actual_attention_id = source
            .as_ref()
            .ok()
            .and_then(|delivery| {
                delivery
                    .snapshot()
                    .attention_items
                    .iter()
                    .find(|item| item.id == requested_attention_id)
                    .or_else(|| {
                        delivery.snapshot().verdict.as_ref().and_then(|_| {
                            let open = delivery
                                .snapshot()
                                .attention_items
                                .iter()
                                .filter(|item| {
                                    item.status
                                        == winwincode_delivery::domain::AttentionItemStatus::Open
                                        && item.blocking
                                })
                                .collect::<Vec<_>>();
                            let [item] = open.as_slice() else {
                                return None;
                            };
                            Some(*item)
                        })
                    })
                    .map(|item| item.id.clone())
            })
            .unwrap_or(requested_attention_id);
        let now_millis = match source.as_ref() {
            Ok(delivery) => self
                .clock
                .mutation_time(delivery.snapshot().updated_at_millis),
            Err(_) => self.clock.peek_next(),
        };
        let plan_review = source.as_ref().ok().and_then(|delivery| {
            let item = delivery
                .snapshot()
                .attention_items
                .iter()
                .find(|item| item.id == actual_attention_id)?;
            let stage = item.stage_run_id.as_ref().and_then(|stage_run_id| {
                delivery
                    .snapshot()
                    .stage_runs
                    .iter()
                    .find(|run| &run.id == stage_run_id)
            })?;
            (item.item_type == AttentionItemType::DecisionRequired
                && stage.stage == DeliveryStage::PlanReview)
                .then_some((delivery, item))
        });
        let (resolution, review_set_sha256) = if let Some((delivery, item)) = plan_review {
            let actor = canonical_id("sys_", &self.id);
            let settled = settle_solution_review_fixture(
                delivery,
                &actor,
                now_millis,
                solution_review_decision(required_str(payload, "resolution")?)?,
            )
            .map_err(string_error)?;
            let review_set_sha256 = settled.review_set_sha256().to_owned();
            let resolution = settled
                .transition()
                .delivery()
                .snapshot()
                .attention_items
                .iter()
                .find(|resolved| resolved.id == item.id)
                .and_then(|resolved| resolved.resolution.clone())
                .ok_or_else(|| {
                    "settled solution-review fixture did not preserve its resolution".to_owned()
                })?;
            (resolution, Some(review_set_sha256))
        } else {
            (required_str(payload, "resolution")?.to_owned(), None)
        };
        let request = command_envelope(
            &self.id,
            "delivery.resolve_attention",
            request_id,
            expected,
            json!({
                "deliveryId": delivery_id,
                "attentionItemId": actual_attention_id,
                "decision": decision,
                "resolution": resolution,
                "remediation": payload.get("remediation").cloned().unwrap_or(Value::Null),
            }),
        );
        let typed = strict_command_envelope(&request)?;
        let transition = source.and_then(|delivery| {
            let attention_id = actual_attention_id;
            let item = delivery
                .snapshot()
                .attention_items
                .iter()
                .find(|item| item.id == attention_id)
                .ok_or_else(|| {
                    format!(
                        "WRONG_STATE:Attention item {} was not found in status {:?}; current items: {:?}",
                        attention_id.0,
                        delivery.snapshot().status,
                        delivery
                            .snapshot()
                            .attention_items
                            .iter()
                            .map(|item| (&item.id.0, item.status))
                            .collect::<Vec<_>>()
                    )
                })?;
            let stage_run_id = item
                .stage_run_id
                .clone()
                .ok_or_else(|| "WRONG_STATE:Attention item has no StageRun".to_owned())?;
            let actor = canonical_id("sys_", &self.id);
            let transition = resolve_attention(
                &delivery,
                ResolveAttentionInput {
                    expected_revision: expected,
                    attention_item_id: attention_id,
                    stage_run_id,
                    expected_context: item.context.clone(),
                    actor,
                    decision: if decision == "resolve" {
                        AttentionDecision::Resolved
                    } else {
                        AttentionDecision::Dismissed
                    },
                    resolution: resolution.clone(),
                    now_millis,
                },
            );
            Ok(transition)
        });
        let (response, resolved) = match transition {
            Ok(Ok(transition)) => {
                let facts = delivery_attention_command_facts(
                    &typed,
                    self.repository_facts_fixture()?,
                    transition,
                )
                .map_err(string_error)?;
                match self
                    .control_plane_mut()
                    .commit_delivery_command(&typed, &facts)
                {
                    Ok(_receipt) => {
                        let delivery = self.query(&delivery_id)?;
                        self.revision_map
                            .insert(legacy_expected.saturating_add(1), delivery.revision());
                        self.refresh_projection_authority()?;
                        (
                            completed_response(
                                "delivery.resolve_attention",
                                request_id,
                                expected,
                                &delivery,
                                &self.id,
                            )?,
                            Some(delivery),
                        )
                    }
                    Err(error) => (
                        delivery_command_error_response(
                            request_id,
                            &error,
                            self.current_revision(&delivery_id),
                        )?,
                        None,
                    ),
                }
            }
            Ok(Err(error)) => (
                coordination_error_response(
                    request_id,
                    &error,
                    self.current_revision(&delivery_id),
                )?,
                None,
            ),
            Err(error) => return Err(error),
        };
        let mut commands = vec![command_entry(
            source_index,
            "control-plane.command",
            request,
            response,
        )];
        if let (Some(delivery), Some(review_set_sha256)) = (resolved, review_set_sha256) {
            commands.push(self.approve_task_breakdown(
                source_index,
                legacy_expected.saturating_add(1),
                &delivery,
                &review_set_sha256,
                request_id,
            )?);
        }
        Ok(commands)
    }

    fn approve_task_breakdown(
        &mut self,
        source_index: usize,
        legacy_revision_after_resolution: u64,
        delivery: &Delivery,
        review_set_sha256: &str,
        parent_request_id: &str,
    ) -> Result<Value, String> {
        let request_id = format!("{parent_request_id}:task-breakdown");
        let expected = delivery.revision();
        let request = command_envelope(
            &self.id,
            "delivery.approve_task_breakdown",
            &request_id,
            expected,
            json!({
                "deliveryId": delivery.id(),
                "reviewSetSha256": format!("sha256:{review_set_sha256}"),
            }),
        );
        let typed = strict_command_envelope(&request)?;
        let result = self
            .control_plane_mut()
            .commit_delivery_task_breakdown(&typed);
        let response = match result {
            Ok(_receipt) => {
                let promoted = self.query(delivery.id())?;
                self.revision_map
                    .insert(legacy_revision_after_resolution, promoted.revision());
                self.refresh_projection_authority()?;
                completed_response(
                    "delivery.approve_task_breakdown",
                    &request_id,
                    expected,
                    &promoted,
                    &self.id,
                )?
            }
            Err(error) => {
                commit_error_response(&request_id, &error, self.current_revision(delivery.id()))?
            }
        };
        Ok(command_entry(
            source_index,
            "control-plane.command",
            request,
            response,
        ))
    }

    #[allow(
        clippy::too_many_lines,
        reason = "the terminal Worker message, candidate seal, and verdict stay visibly ordered"
    )]
    fn submit_verdict(
        &mut self,
        source_index: usize,
        legacy: &Value,
    ) -> Result<Vec<Value>, String> {
        let payload = object(legacy, "payload")?;
        self.execution_source = parse_execution_source(payload);
        let delivery_id = DeliveryId(required_str(payload, "deliveryId")?.to_owned());
        let legacy_expected = required_u64(payload, "expectedRevision")?;
        let request_id = required_str(legacy, "requestId")?;
        let mut commands = Vec::with_capacity(2);
        if let Some((terminal, committed)) = self.commit_terminal_verifier_fact(
            source_index,
            payload,
            &delivery_id,
            legacy_expected,
        )? {
            commands.push(terminal);
            if !committed {
                return Ok(commands);
            }
        }
        let expected = self.actual_revision(legacy_expected);
        let candidate_input = object(payload, "candidate")?;
        let source_candidate_ref = required_str(candidate_input, "candidateRef")?.to_owned();
        let delivery = self.query(&delivery_id)?;
        let candidate = if let Some(candidate) = self.candidates.get(&source_candidate_ref) {
            candidate.clone()
        } else {
            self.freeze_candidate_input(&delivery, candidate_input)?
        };
        self.candidates
            .entry(source_candidate_ref)
            .or_insert_with(|| candidate.clone());
        self.execution_source.candidate_fact = Some(candidate.clone());
        self.projection_authority.replace(
            &self.repository_scope,
            &delivery,
            &self.execution_source,
        )?;
        let digest = candidate
            .candidate_ref()
            .strip_prefix("git-candidate:")
            .ok_or_else(|| "sealed candidate ref lacks git-candidate prefix".to_owned())?;
        let request = command_envelope(
            &self.id,
            "delivery.submit_verdict",
            request_id,
            expected,
            json!({
                "deliveryId": delivery_id,
                "candidateDigest": digest,
            }),
        );
        let typed = strict_command_envelope(&request)?;
        let (facts_candidate, semantic_candidate_input) = if delivery
            .snapshot()
            .stage_runs
            .iter()
            .rev()
            .find(|run| {
                matches!(
                    run.stage,
                    DeliveryStage::Executing | DeliveryStage::Reworking
                )
            })
            .is_some_and(|writer| &writer.id != candidate.producer_stage_run_id())
        {
            let writer = delivery
                .snapshot()
                .stage_runs
                .iter()
                .rev()
                .find(|run| {
                    matches!(
                        run.stage,
                        DeliveryStage::Executing | DeliveryStage::Reworking
                    )
                })
                .expect("writer was selected above");
            let planned = self
                .planned_candidates
                .get(&writer.id.0)
                .cloned()
                .ok_or_else(|| {
                    format!(
                        "stale verdict lacks a sealed replacement candidate for writer {}",
                        writer.id.0
                    )
                })?;
            let current = self.freeze_candidate_input(&delivery, &planned)?;
            self.candidates
                .entry(required_str(&planned, "candidateRef")?.to_owned())
                .or_insert_with(|| current.clone());
            (current, planned)
        } else {
            (candidate.clone(), candidate_input.clone())
        };
        let legacy_spec_identity = self
            .legacy_spec_identity
            .clone()
            .ok_or_else(|| "submitVerdict lacks the current migrated Spec identity".to_owned())?;
        let semantic_authority = verification_semantic_authority(
            &delivery,
            &facts_candidate,
            &semantic_candidate_input,
            &legacy_spec_identity,
        )?;
        let outcome = verdict_outcome_from_semantics(payload, &semantic_authority)?;
        let sealed = verdict_facts_fixture(&delivery, &facts_candidate, outcome);
        let produced_at_millis = self.verdict_time(&delivery);
        let result = self.control_plane_mut().commit_delivery_verdict(
            &typed,
            SubmitVerdictFacts {
                expected_revision: expected,
                candidate: &candidate,
                verification: sealed.verification(),
                evidence: sealed.evidence(),
                produced_at_millis,
            },
        );
        let response = match result {
            Ok(_receipt) => {
                let delivery = self.query(&delivery_id)?;
                self.revision_map
                    .insert(legacy_expected.saturating_add(1), delivery.revision());
                self.refresh_projection_authority()?;
                completed_response(
                    "delivery.submit_verdict",
                    request_id,
                    expected,
                    &delivery,
                    &self.id,
                )?
            }
            Err(error) => {
                verdict_error_response(request_id, &error, self.current_revision(&delivery_id))?
            }
        };
        commands.push(command_entry(
            source_index,
            "control-plane.command",
            request,
            response,
        ));
        Ok(commands)
    }

    fn freeze_candidate_input(
        &self,
        delivery: &Delivery,
        candidate: &Value,
    ) -> Result<FrozenDeliveryCandidate, String> {
        let producer_stage_run_id =
            canonical_stage_run_id(required_str(candidate, "producerStageRunId")?);
        let producer_session_binding_id =
            canonical_session_binding_id(required_str(candidate, "producerSessionBindingId")?)?;
        let input = candidate_fixture_input(candidate)?;
        let producer_is_rework = delivery
            .snapshot()
            .stage_runs
            .iter()
            .find(|run| run.id == producer_stage_run_id)
            .is_some_and(|run| run.stage == DeliveryStage::Reworking);
        if producer_is_rework {
            let authorization = self.rework_authorization.as_ref().ok_or_else(|| {
                "replacement candidate lacks its sealed rework authorization".to_owned()
            })?;
            Ok(freeze_rework_replacement_candidate_fixture(
                delivery,
                authorization,
                &producer_stage_run_id,
                &producer_session_binding_id,
                canonical_rework_candidate_input(input, authorization)?,
            ))
        } else {
            Ok(freeze_candidate_fixture(
                delivery,
                &producer_stage_run_id,
                &producer_session_binding_id,
                input,
            ))
        }
    }

    #[allow(
        clippy::too_many_lines,
        reason = "one generated Worker message is joined to its exact sealed lease and receipt"
    )]
    fn commit_terminal_verifier_fact(
        &mut self,
        source_index: usize,
        payload: &Value,
        delivery_id: &DeliveryId,
        legacy_expected: u64,
    ) -> Result<Option<(Value, bool)>, String> {
        let fact = terminal_verifier_fact(payload)?;
        if self.terminal_facts.contains(&fact.key) {
            return Ok(None);
        }
        let outcome_status = self
            .terminal_outcome_statuses
            .remove(&source_index)
            .ok_or_else(|| {
                format!(
                    "terminal verifier source {source_index} lacks its closed plan outcome status"
                )
            })?;
        let delivery = self.query(delivery_id)?;
        let active = delivery
            .snapshot()
            .stage_runs
            .iter()
            .filter(|run| {
                run.stage == DeliveryStage::Verifying
                    && run.role == "verifier"
                    && run.actor_type == StageRunActorType::Codex
                    && matches!(
                        run.status,
                        StageRunStatus::Running | StageRunStatus::Waiting
                    )
            })
            .collect::<Vec<_>>();
        let [run] = active.as_slice() else {
            return Err(format!(
                "terminal verifier fact requires one current active verifier, found {}",
                active.len()
            ));
        };
        let fixture = self
            .leases
            .get(&run.id.0)
            .cloned()
            .ok_or_else(|| "active verifier lacks its sealed lease fixture".to_owned())?;
        if fixture.legacy_session_id != fact.key.legacy_session_id {
            return Err(format!(
                "terminal verifier session {} does not match active verifier {}",
                fact.key.legacy_session_id, fixture.legacy_session_id
            ));
        }
        let binding = delivery
            .snapshot()
            .session_bindings
            .iter()
            .find(|binding| binding.stage_run_id == run.id)
            .ok_or_else(|| "active verifier lacks its exact SessionBinding".to_owned())?;
        let worker_session_id = binding
            .worker_session_id
            .as_ref()
            .ok_or_else(|| "active verifier SessionBinding lacks WorkerSession".to_owned())?;
        let codex_thread_id = binding
            .codex_thread_id
            .as_ref()
            .ok_or_else(|| "active verifier SessionBinding lacks CodexThread".to_owned())?;
        if worker_session_id != &fixture.binding_message.worker_session_id
            || codex_thread_id != &fixture.binding_message.codex_thread_id
            || binding.execution_job_id != fixture.binding_message.lease.job_id
        {
            return Err(
                "terminal verifier fact does not join the accepted session-binding message"
                    .to_owned(),
            );
        }
        let finished_at = millis_to_rfc3339(fact.key.occurred_at_millis)?;
        let message_seed = format!(
            "winwincode.delivery-strongflow-terminal-outcome.v1\0{}\0{}\0{}\0{}",
            delivery_id.0,
            fact.key.legacy_session_id,
            fact.key.last_event_sequence,
            fact.key.occurred_at_millis
        );
        let request = json!({
            "schemaVersion": API_SCHEMA,
            "messageId": canonical_id("xmsg_", &message_seed),
            "kind": "job.outcome",
            "sentAt": finished_at,
            "lease": fixture.binding_message.lease,
            "workerSessionId": worker_session_id,
            "outcome": {
                "artifacts": [],
                "codexThreadId": codex_thread_id,
                "finishedAt": finished_at,
                "lastEventSequence": fact.key.last_event_sequence,
                "status": outcome_status,
                "summary": fact.summary,
            },
        });
        let message: JobOutcomeMessage = serde_json::from_value(request.clone())
            .map_err(|error| format!("canonical JobOutcomeMessage rejected: {error}"))?;
        if serde_json::to_value(&message).map_err(string_error)? != request {
            return Err("JobOutcomeMessage did not round-trip exactly".to_owned());
        }
        let last_event_sequence = ExecutionAckSequence(
            i64::try_from(fact.key.last_event_sequence).map_err(string_error)?,
        );
        let outcome = terminal_worker_outcome(
            run.id.clone(),
            message.lease.job_id.clone(),
            u64::try_from(message.lease.attempt).map_err(string_error)?,
            message.lease.lease_id.clone(),
            message.lease.fencing_token.clone(),
            message.lease.worker_id.clone(),
            message.lease.worker_instance_id.clone(),
            message.worker_session_id.clone(),
            delivery_terminal_outcome_status(&message.outcome.status),
            terminal_outcome_metadata(
                message.outcome.codex_thread_id.clone(),
                fact.key.occurred_at_millis,
                last_event_sequence,
                Vec::<TerminalArtifactReference>::new(),
            ),
        );
        let facts = delivery_terminal_outcome_facts(fixture.authority, outcome);
        let repository_scope = self.repository_scope.clone();
        let result = self.control_plane_mut().commit_delivery_terminal_outcome(
            &repository_scope,
            &message,
            &facts,
        );
        let (response, committed) = match result {
            Ok(commit) => {
                let receipt = commit.receipt();
                if receipt.revision != delivery.revision().saturating_add(1) {
                    return Err("terminal outcome receipt is not the next Delivery revision".into());
                }
                self.terminal_facts.insert(fact.key);
                self.revision_map.insert(legacy_expected, receipt.revision);
                self.refresh_projection_authority()?;
                (
                    json!({
                        "messageId": message.message_id,
                        "outcome": "completed",
                        "previousRevision": delivery.revision(),
                        "currentRevision": receipt.revision,
                        "commits": [{
                            "operation": "apply_terminal_outcome",
                            "previousRevision": delivery.revision(),
                            "currentRevision": receipt.revision,
                            "receipt": receipt_json(receipt)?,
                        }],
                    }),
                    true,
                )
            }
            Err(error) => (
                terminal_outcome_error_response(
                    &message.message_id.0,
                    &error,
                    self.current_revision(delivery_id),
                )?,
                false,
            ),
        };
        Ok(Some((
            command_entry(source_index, "execution-port.message", request, response),
            committed,
        )))
    }

    fn verdict_time(&mut self, delivery: &Delivery) -> u64 {
        let observed = self
            .execution_source
            .runtime_events
            .iter()
            .filter_map(|event| event.get("occurredAtMillis").and_then(Value::as_u64))
            .max()
            .unwrap_or(0);
        self.clock
            .mutation_time(delivery.snapshot().updated_at_millis.max(observed))
    }

    fn get_projection(
        &mut self,
        source_index: usize,
        legacy: &Value,
    ) -> Result<Vec<Value>, String> {
        let payload = object(legacy, "payload")?;
        let delivery_id = DeliveryId(required_str(payload, "deliveryId")?.to_owned());
        let request_id = required_str(legacy, "requestId")?;
        let delivery_request_input = query_envelope(
            &self.id,
            "delivery.get",
            request_id,
            json!({ "deliveryId": delivery_id, "atCursor": null }),
        );
        let typed_delivery: DeliveryGetQuery = serde_json::from_value(delivery_request_input)
            .map_err(|error| format!("canonical delivery.get query rejected: {error}"))?;
        let delivery_request = serde_json::to_value(&typed_delivery).map_err(string_error)?;
        if self.query(&delivery_id).is_ok() {
            self.refresh_projection_authority()?;
        }
        match self.control_plane().delivery_get(&typed_delivery) {
            Ok(delivery_result) => {
                let binding = last_complete_projection_binding(
                    &delivery_projection_result(&delivery_result)?.stages,
                );
                let delivery_response = strict_query_result(delivery_result)?;
                let cursor = delivery_response
                    .pointer("/result/readCursor")
                    .cloned()
                    .ok_or_else(|| "delivery.get result lacks readCursor".to_owned())?;
                let Some(binding) = binding else {
                    return Ok(vec![command_entry(
                        source_index,
                        "control-plane.query",
                        delivery_request,
                        delivery_response,
                    )]);
                };
                let runtime_request_input = query_envelope(
                    &self.id,
                    "runtime.projection.get",
                    &format!("{request_id}:runtime"),
                    json!({
                        "kind": "delivery-stage",
                        "deliveryId": delivery_id,
                        "stageRunId": binding.stage_run_id,
                        "productSessionId": binding.product_session_id,
                        "atCursor": cursor,
                    }),
                );
                let typed_runtime: RuntimeProjectionGetQuery =
                    serde_json::from_value(runtime_request_input).map_err(|error| {
                        format!("canonical runtime.projection.get query rejected: {error}")
                    })?;
                let runtime_request = serde_json::to_value(&typed_runtime).map_err(string_error)?;
                let runtime_response =
                    match self.control_plane().runtime_projection_get(&typed_runtime) {
                        Ok(result) => {
                            validate_runtime_projection_result(&result, &binding)?;
                            strict_query_result(result)?
                        }
                        Err(error) => {
                            projection_error_response(&format!("{request_id}:runtime"), &error)?
                        }
                    };
                Ok(vec![
                    command_entry(
                        source_index,
                        "control-plane.query",
                        delivery_request,
                        delivery_response,
                    ),
                    command_entry(
                        source_index,
                        "control-plane.query",
                        runtime_request,
                        runtime_response,
                    ),
                ])
            }
            Err(error) => Ok(vec![command_entry(
                source_index,
                "control-plane.query",
                delivery_request,
                projection_error_response(request_id, &error)?,
            )]),
        }
    }

    fn typed_projection_pair(
        &self,
        delivery: &Delivery,
        request_suffix: &str,
    ) -> Result<(Value, Value), String> {
        let request_id = format!("oracle:{}:{request_suffix}:delivery", self.id);
        let delivery_request = query_envelope(
            &self.id,
            "delivery.get",
            &request_id,
            json!({ "deliveryId": delivery.id(), "atCursor": null }),
        );
        let delivery_query: DeliveryGetQuery =
            serde_json::from_value(delivery_request).map_err(string_error)?;
        let delivery_result = self
            .control_plane()
            .delivery_get(&delivery_query)
            .map_err(string_error)?;
        let binding =
            last_complete_projection_binding(&delivery_projection_result(&delivery_result)?.stages);
        let delivery_response = strict_query_result(delivery_result)?;
        let delivery_projection = delivery_response
            .get("result")
            .cloned()
            .ok_or_else(|| "observation delivery projection is missing".to_owned())?;
        let cursor = delivery_response
            .pointer("/result/readCursor")
            .cloned()
            .ok_or_else(|| "observation delivery cursor is missing".to_owned())?;
        let Some(binding) = binding else {
            return Ok((delivery_projection, Value::Null));
        };
        let runtime_request = query_envelope(
            &self.id,
            "runtime.projection.get",
            &format!("{request_id}:runtime"),
            json!({
                "kind": "delivery-stage",
                "deliveryId": delivery.id(),
                "stageRunId": binding.stage_run_id,
                "productSessionId": binding.product_session_id,
                "atCursor": cursor,
            }),
        );
        let runtime_query: RuntimeProjectionGetQuery =
            serde_json::from_value(runtime_request).map_err(string_error)?;
        let runtime_result = self
            .control_plane()
            .runtime_projection_get(&runtime_query)
            .map_err(string_error)?;
        validate_runtime_projection_result(&runtime_result, &binding)?;
        let runtime_response = strict_query_result(runtime_result)?;
        let runtime_projection = runtime_response
            .get("result")
            .cloned()
            .ok_or_else(|| "observation runtime projection is missing".to_owned())?;
        Ok((delivery_projection, runtime_projection))
    }

    fn spec_facts_fixture(
        &self,
        spec: &Value,
        now_millis: u64,
    ) -> Result<DeliverySpecFactsFixture, String> {
        let repository: RepositoryRef =
            serde_json::from_value(object(spec, "repository")?.clone()).map_err(string_error)?;
        let source_ref =
            serde_json::from_value(spec.get("sourceRef").cloned().unwrap_or(Value::Null))
                .map_err(string_error)?;
        let strings = |field: &str| -> Result<Vec<String>, String> {
            serde_json::from_value(
                spec.get(field)
                    .cloned()
                    .ok_or_else(|| format!("DeliverySpec lacks {field}"))?,
            )
            .map_err(string_error)
        };
        let criteria = spec
            .get("acceptanceCriteria")
            .and_then(Value::as_array)
            .ok_or_else(|| "DeliverySpec acceptanceCriteria must be an array".to_owned())?;
        let criterion_verification_methods = criteria
            .iter()
            .map(|criterion| {
                Ok((
                    required_str(criterion, "id")?.to_owned(),
                    required_str(criterion, "verificationMethod")?.to_owned(),
                ))
            })
            .collect::<Result<Vec<_>, String>>()?;
        Ok(DeliverySpecFactsFixture {
            repository_scope: self.repository_scope.clone(),
            now_millis,
            repository,
            source_ref,
            scope: strings("scope")?,
            out_of_scope: strings("outOfScope")?,
            constraints: strings("constraints")?,
            max_rework_attempts: required_u64(spec, "maxReworkAttempts")?,
            criterion_verification_methods,
        })
    }

    fn repository_facts_fixture(&self) -> Result<DeliveryRepositoryFactsFixture, String> {
        let delivery_id = self
            .delivery_id
            .as_ref()
            .ok_or_else(|| "repository facts require a Delivery".to_owned())?;
        let delivery = self.query(delivery_id)?;
        Ok(DeliveryRepositoryFactsFixture {
            repository_scope: self.repository_scope.clone(),
            repository: delivery.snapshot().spec.repository.clone(),
            source_ref: delivery.snapshot().spec.source_ref.clone(),
        })
    }

    fn current_verdict_candidate(
        &self,
        delivery: &Delivery,
    ) -> Result<FrozenDeliveryCandidate, String> {
        let candidate_ref = delivery
            .snapshot()
            .verdict
            .as_ref()
            .ok_or_else(|| "rework dispatch requires a current failed verdict".to_owned())?
            .candidate_ref
            .as_str();
        self.candidates
            .values()
            .chain(self.execution_source.candidate_fact.iter())
            .find(|candidate| candidate.candidate_ref() == candidate_ref)
            .cloned()
            .ok_or_else(|| {
                format!("rework dispatch lacks the sealed current candidate {candidate_ref}")
            })
    }

    fn execution_config(
        &self,
        delivery_id: &DeliveryId,
        transition: &StageAdvanceResult,
    ) -> Result<DeliveryExecutionConfig, String> {
        let delivery = self.query(delivery_id)?;
        execution_config_for_transition(&delivery, transition, &self.repository_scope.repository_id)
    }

    fn current_revision(&self, delivery_id: &DeliveryId) -> Option<u64> {
        self.query(delivery_id)
            .ok()
            .map(|delivery| delivery.revision())
    }

    fn actual_revision(&self, legacy: u64) -> u64 {
        self.revision_map.get(&legacy).copied().unwrap_or(legacy)
    }

    fn query(&self, delivery_id: &DeliveryId) -> Result<Delivery, String> {
        let state = self
            .control_plane()
            .load_state(&format!("delivery:{}", delivery_id.0))
            .map_err(string_error)?
            .ok_or_else(|| format!("Delivery {} was not found", delivery_id.0))?;
        Delivery::decode_json(&state.payload).map_err(string_error)
    }

    fn control_plane(&self) -> &ControlPlane {
        self.control_plane
            .as_ref()
            .expect("running differential scenario owns its Control Plane")
    }

    fn control_plane_mut(&mut self) -> &mut ControlPlane {
        self.control_plane
            .as_mut()
            .expect("running differential scenario owns its Control Plane")
    }

    fn restart_control_plane(&mut self) -> Result<(), String> {
        self.stop_control_plane()?;
        self.start_control_plane()?;
        self.refresh_projection_authority()
    }

    fn stop_control_plane(&mut self) -> Result<(), String> {
        let control_plane = self
            .control_plane
            .take()
            .ok_or_else(|| "differential Control Plane is not running".to_owned())?;
        control_plane.shutdown().map_err(string_error)?;
        Ok(())
    }

    fn start_control_plane(&mut self) -> Result<(), String> {
        if self.control_plane.is_some() {
            return Err("differential Control Plane is already running".to_owned());
        }
        let mut control_plane = ControlPlane::start_local(
            ControlPlaneConfig::local(&self.home),
            Box::new(CapturingPublisher {
                events: Arc::clone(&self.published),
            }),
        )
        .map_err(string_error)?;
        control_plane
            .install_strongflow_projection_sources(self.projection_authority.sources())
            .map_err(string_error)?;
        self.control_plane = Some(control_plane);
        Ok(())
    }

    fn refresh_projection_authority(&self) -> Result<(), String> {
        let Some(delivery_id) = self.delivery_id.as_ref() else {
            return Ok(());
        };
        let delivery = self.query(delivery_id)?;
        self.projection_authority
            .replace(&self.repository_scope, &delivery, &self.execution_source)
    }

    fn terminal_handoff(
        &self,
        delivery: &Delivery,
    ) -> Result<
        (
            Option<winwincode_delivery::application::stage::VerifiedTerminalOutcome>,
            Option<winwincode_delivery::application::stage::ActiveLeaseIdentity>,
        ),
        String,
    > {
        let Some(run) = delivery.snapshot().stage_runs.iter().find(|run| {
            matches!(
                run.status,
                StageRunStatus::Running | StageRunStatus::Waiting
            )
        }) else {
            return Ok((None, None));
        };
        if run.actor_type == StageRunActorType::Human {
            return Ok((None, None));
        }
        let fixture = self
            .leases
            .get(&run.id.0)
            .ok_or_else(|| "WRONG_STATE:active stage has no accepted Worker session".to_owned())?;
        let binding = delivery
            .snapshot()
            .session_bindings
            .iter()
            .find(|binding| binding.stage_run_id == run.id)
            .ok_or_else(|| "WRONG_STATE:active stage has no SessionBinding".to_owned())?;
        let finished_at = delivery.snapshot().updated_at_millis.saturating_add(1);
        let metadata = terminal_outcome_metadata(
            binding.codex_thread_id.clone(),
            finished_at,
            ExecutionAckSequence(0),
            Vec::<TerminalArtifactReference>::new(),
        );
        let outcome = terminal_worker_outcome(
            run.id.clone(),
            binding.execution_job_id.clone(),
            run.attempt,
            fixture.authority.active_lease().lease_id().clone(),
            fixture.authority.active_lease().fencing_token().clone(),
            fixture.authority.active_lease().worker_id().clone(),
            fixture
                .authority
                .active_lease()
                .worker_instance_id()
                .clone(),
            fixture.binding_message.worker_session_id.clone(),
            TerminalOutcomeStatus::Succeeded,
            metadata,
        );
        let verified = verify_terminal_outcome(delivery, fixture.authority.active_lease(), outcome)
            .map_err(string_error)?;
        Ok((
            Some(verified),
            Some(fixture.authority.active_lease().clone()),
        ))
    }

    fn observe(&self) -> Result<Value, String> {
        let delivery_id = self
            .delivery_id
            .as_ref()
            .ok_or_else(|| format!("scenario {} never established a Delivery", self.id))?;
        let delivery = self.query(delivery_id)?;
        self.refresh_projection_authority()?;
        let (delivery_projection, runtime_projection) =
            self.typed_projection_pair(&delivery, "observation")?;
        let stored = sqlite_durable_observation(&self.home, delivery_id)?;
        Ok(json!({
            "events": self.execution_source.runtime_events,
            "projection": {
                "delivery": delivery_projection,
                "runtime": runtime_projection,
            },
            "snapshot": serde_json::to_value(delivery.snapshot()).map_err(string_error)?,
            "store": stored,
            "verdict": serde_json::to_value(&delivery.snapshot().verdict).map_err(string_error)?,
        }))
    }

    fn require_all_terminal_outcome_statuses_consumed(&self) -> Result<(), String> {
        if self.terminal_outcome_statuses.is_empty() {
            return Ok(());
        }
        Err(format!(
            "scenario {} left terminal outcome statuses unconsumed at source indexes {:?}",
            self.id,
            self.terminal_outcome_statuses.keys().collect::<Vec<_>>()
        ))
    }
}

#[derive(Clone, Copy)]
enum LegacyOperation {
    Create,
    UpdateSpec,
    StartStage,
    BindSession,
    ResolveAttention,
    SubmitVerdict,
    GetProjection,
}

impl FromStr for LegacyOperation {
    type Err = String;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        match value {
            "createDelivery" => Ok(Self::Create),
            "updateDeliverySpec" => Ok(Self::UpdateSpec),
            "startStage" => Ok(Self::StartStage),
            "bindSession" => Ok(Self::BindSession),
            "resolveAttention" => Ok(Self::ResolveAttention),
            "submitVerdict" => Ok(Self::SubmitVerdict),
            "getDeliveryProjection" => Ok(Self::GetProjection),
            other => Err(format!("unsupported frozen transcript operation {other}")),
        }
    }
}

#[derive(Default)]
struct FixtureClock {
    last: u64,
}

impl FixtureClock {
    fn create_time(&mut self, spec_created_at: u64) -> u64 {
        let next = spec_created_at.saturating_add(100);
        self.last = self.last.max(next);
        self.last
    }

    fn mutation_time(&mut self, current: u64) -> u64 {
        self.last = self.last.max(current).saturating_add(1);
        self.last
    }

    fn peek_next(&self) -> u64 {
        self.last.saturating_add(1)
    }
}

fn solution_review_fixture(attention: &Value) -> Result<SolutionReviewFixture, String> {
    let context: Value =
        serde_json::from_str(required_str(attention, "context")?).map_err(string_error)?;
    Ok(SolutionReviewFixture {
        attention_title: required_str(attention, "title")?.to_owned(),
        assigned_to: required_str(attention, "assignedTo")?.to_owned(),
        solution: serde_json::from_value::<SolutionFixture>(
            context
                .get("solution")
                .cloned()
                .ok_or_else(|| "solution-review fixture lacks solution".to_owned())?,
        )
        .map_err(string_error)?,
        architecture_diagram: serde_json::from_value::<SolutionDiagramFixture>(
            context
                .get("architectureDiagram")
                .cloned()
                .ok_or_else(|| "solution-review fixture lacks architectureDiagram".to_owned())?,
        )
        .map_err(string_error)?,
        process_diagram: serde_json::from_value::<SolutionDiagramFixture>(
            context
                .get("processDiagram")
                .cloned()
                .ok_or_else(|| "solution-review fixture lacks processDiagram".to_owned())?,
        )
        .map_err(string_error)?,
        risks: serde_json::from_value(context.get("risks").cloned().unwrap_or_else(|| json!([])))
            .map_err(string_error)?,
        unresolved_items: serde_json::from_value(
            context
                .get("unresolvedItems")
                .cloned()
                .unwrap_or_else(|| json!([])),
        )
        .map_err(string_error)?,
        task_proposals: Vec::new(),
    })
}

fn invalid_cycle_review_fixture(
    delivery: &Delivery,
    scenario: &str,
) -> Result<SolutionReviewFixture, String> {
    let diagram = |id: &str, kind: &str| -> Result<SolutionDiagramFixture, String> {
        serde_json::from_value(json!({
            "id": id,
            "kind": kind,
            "title": format!("{id} fixture"),
            "nodes": [
                {
                    "id": format!("{id}:input"),
                    "label": "Input",
                    "description": "Starts the cycle migration fixture.",
                    "kind": "stage",
                    "trustBoundary": null,
                    "unresolved": false
                },
                {
                    "id": format!("{id}:output"),
                    "label": "Output",
                    "description": "Completes the cycle migration fixture.",
                    "kind": "decision",
                    "trustBoundary": "fixture-review",
                    "unresolved": false
                }
            ],
            "edges": [{
                "id": format!("{id}:edge"),
                "from": format!("{id}:input"),
                "to": format!("{id}:output"),
                "label": "reviews"
            }]
        }))
        .map_err(string_error)
    };
    Ok(SolutionReviewFixture {
        attention_title: "Reject the cyclic task proposal".to_owned(),
        assigned_to: canonical_id("sys_", scenario),
        solution: serde_json::from_value::<SolutionFixture>(json!({
            "id": "solution:task-dag-cycle",
            "summary": "Exercise the canonical cyclic task rejection.",
            "approach": ["Keep the ordered task graph sealed by the review fixture."],
            "components": [{
                "id": "component:task-dag-cycle",
                "label": "Task graph",
                "responsibility": "Carries the reviewed task dependencies.",
                "kind": "component",
                "trustBoundary": "repository",
                "unresolved": false,
                "repositoryPathPrefixes": ["src"]
            }],
            "connections": [{
                "id": "connection:task-dag-cycle",
                "from": "platform:codex-core",
                "to": "component:task-dag-cycle",
                "label": "reviews"
            }]
        }))
        .map_err(string_error)?,
        architecture_diagram: diagram(
            "diagram:task-dag-cycle-architecture",
            "system-architecture",
        )?,
        process_diagram: diagram("diagram:task-dag-cycle-process", "process-flow")?,
        risks: vec!["A cycle would make the task graph unrunnable.".to_owned()],
        unresolved_items: Vec::new(),
        task_proposals: invalid_task_proposals_fixture(
            delivery,
            InvalidTaskProposalFixture::DependencyCycle,
        ),
    })
}

fn cycle_validation_delivery(spec: &Value) -> Result<(Delivery, AdvanceStageInput), String> {
    let delivery_id = DeliveryId(required_str(spec, "deliveryId")?.to_owned());
    let started_at = required_u64(spec, "createdAtMillis")?.saturating_add(100);
    let planning_stage_id = canonical_stage_run_id("oracle-task-dag-cycle-planning");
    let planning_binding_id = canonical_session_binding_id(&planning_stage_id.0)?;
    let planning_job_id = ExecutionJobId(canonical_id("job_", &planning_stage_id.0));
    let planning_product_session_id = ProductSessionId(canonical_id("psn_", &planning_stage_id.0));
    let worker_session_id = WorkerSessionId(canonical_id("wsn_", &planning_stage_id.0));
    let codex_thread_id = CodexThreadId(canonical_id("cdx_", &planning_stage_id.0));
    let delivery = Delivery::decode_json(
        &serde_json::to_vec(&json!({
            "schemaVersion": 3,
            "id": delivery_id,
            "revision": 1,
            "spec": spec,
            "tasks": [],
            "stageRuns": [{
                "schemaVersion": 3,
                "id": planning_stage_id,
                "deliveryId": delivery_id,
                "deliveryTaskId": null,
                "stage": "planning",
                "actorType": "codex",
                "role": "planner",
                "attempt": 1,
                "status": "running",
                "startedAtMillis": started_at,
                "finishedAtMillis": null,
            }],
            "sessionBindings": [{
                "schemaVersion": 3,
                "id": planning_binding_id,
                "deliveryId": delivery_id,
                "deliveryTaskId": null,
                "stageRunId": planning_stage_id,
                "productSessionId": planning_product_session_id,
                "executionJobId": planning_job_id,
                "workerSessionId": worker_session_id,
                "codexThreadId": codex_thread_id,
                "boundAtMillis": started_at,
            }],
            "attentionItems": [],
            "evidence": [],
            "verdict": null,
            "status": "planning",
            "createdAtMillis": spec["createdAtMillis"],
            "updatedAtMillis": started_at,
        }))
        .map_err(string_error)?,
    )
    .map_err(string_error)?;
    let lease = active_lease_identity(
        planning_job_id,
        1,
        LeaseId(canonical_id("lse_", &planning_stage_id.0)),
        FencingToken("1".to_owned()),
        WorkerId(canonical_id("wrk_", &planning_stage_id.0)),
        WorkerInstanceId(canonical_id("wki_", &planning_stage_id.0)),
        worker_session_id.clone(),
    );
    let terminal = verify_terminal_outcome(
        &delivery,
        &lease,
        terminal_worker_outcome(
            planning_stage_id,
            lease.execution_job_id().clone(),
            1,
            lease.lease_id().clone(),
            lease.fencing_token().clone(),
            lease.worker_id().clone(),
            lease.worker_instance_id().clone(),
            worker_session_id,
            TerminalOutcomeStatus::Succeeded,
            terminal_outcome_metadata(
                Some(codex_thread_id),
                started_at.saturating_add(1),
                ExecutionAckSequence(1),
                Vec::new(),
            ),
        ),
    )
    .map_err(string_error)?;
    let review_stage_id = canonical_stage_run_id("oracle-task-dag-cycle-review");
    let input = AdvanceStageInput {
        expected_revision: delivery.revision(),
        product_session_id: ProductSessionId(canonical_id("psn_", &review_stage_id.0)),
        identities: NewStageIdentities {
            stage_run_id: review_stage_id.clone(),
            execution_job_id: ExecutionJobId(canonical_id("job_", &review_stage_id.0)),
            session_binding_id: canonical_session_binding_id(&review_stage_id.0)?,
            attention_item_id: AttentionItemId(canonical_id("att_", &review_stage_id.0)),
        },
        review: None,
        previous_outcome: Some(terminal),
        current_lease: Some(lease),
        rework_authorization: None,
        now_millis: started_at.saturating_add(1),
    };
    Ok((delivery, input))
}

fn solution_review_decision(raw: &str) -> Result<SolutionReviewDecisionFixture, String> {
    let legacy: Value = serde_json::from_str(raw).map_err(string_error)?;
    let comments = legacy
        .get("comments")
        .and_then(Value::as_str)
        .map(str::to_owned);
    match required_str(&legacy, "action")? {
        "approve" => Ok(SolutionReviewDecisionFixture::Approve { comments }),
        "request_changes" => Ok(SolutionReviewDecisionFixture::RequestChanges {
            comments,
            requested_changes: serde_json::from_value(
                legacy
                    .get("requestedChanges")
                    .cloned()
                    .unwrap_or_else(|| json!([])),
            )
            .map_err(string_error)?,
        }),
        "reject" => Ok(SolutionReviewDecisionFixture::Reject { comments }),
        action => Err(format!("unsupported solution-review decision {action}")),
    }
}

fn canonical_create_payload(
    payload: &Value,
    delivery_id: &DeliveryId,
    repository_id: &str,
) -> Result<Value, String> {
    let spec = object(payload, "spec")?;
    payload
        .get("tasks")
        .and_then(Value::as_array)
        .ok_or_else(|| "create tasks must be an array".to_owned())?;
    Ok(json!({
        "deliveryId": delivery_id,
        "spec": canonical_spec_input(spec, repository_id)?,
        "tasks": [],
    }))
}

fn canonical_update_payload(
    payload: &Value,
    delivery_id: &DeliveryId,
    repository_id: &str,
) -> Result<Value, String> {
    let spec = object(payload, "spec")?;
    Ok(json!({
        "deliveryId": delivery_id,
        "spec": canonical_spec_input(spec, repository_id)?,
    }))
}

fn canonical_spec_input(spec: &Value, repository_id: &str) -> Result<Value, String> {
    let criteria = spec
        .get("acceptanceCriteria")
        .and_then(Value::as_array)
        .ok_or_else(|| "spec acceptanceCriteria must be an array".to_owned())?;
    Ok(json!({
        "acceptanceCriteria": criteria.iter().map(|criterion| {
            Ok::<_, String>(json!({
                "id": required_str(criterion, "id")?,
                "title": required_str(criterion, "description")?,
                "required": criterion.get("required").and_then(Value::as_bool)
                    .ok_or_else(|| "criterion required must be boolean".to_owned())?,
            }))
        }).collect::<Result<Vec<_>, _>>()?,
        "baseRevision": required_str(spec, "baseRevision")?,
        "goal": required_str(spec, "goal")?,
        "publicationTarget": spec.get("publicationTarget").cloned().unwrap_or(Value::Null),
        "repositoryId": repository_id,
        "title": required_str(spec, "title")?,
    }))
}

fn command_envelope(
    scenario: &str,
    command: &str,
    request_id: &str,
    expected_revision: u64,
    payload: Value,
) -> Value {
    let request_id = canonical_request_id(request_id);
    json!({
        "schemaVersion": API_SCHEMA,
        "actor": { "kind": "system", "id": canonical_id("sys_", scenario) },
        "scope": fixture_scope(scenario),
        "requestId": request_id,
        "expectedRevision": expected_revision,
        "command": command,
        "payload": payload,
    })
}

fn strict_command_envelope(value: &Value) -> Result<CommandEnvelope, String> {
    let command: CommandEnvelope = serde_json::from_value(value.clone())
        .map_err(|error| format!("canonical CommandEnvelope rejected: {error}"))?;
    if serde_json::to_value(&command).map_err(string_error)? != *value {
        return Err("canonical CommandEnvelope did not round-trip exactly".to_owned());
    }
    Ok(command)
}

fn query_envelope(scenario: &str, query: &str, request_id: &str, parameters: Value) -> Value {
    let request_id = canonical_request_id(request_id);
    json!({
        "schemaVersion": API_SCHEMA,
        "actor": { "kind": "system", "id": canonical_id("sys_", scenario) },
        "scope": fixture_scope(scenario),
        "requestId": request_id,
        "query": query,
        "parameters": parameters,
        "page": { "cursor": null, "limit": 200 },
    })
}

#[derive(Debug, Clone, PartialEq, Eq)]
#[allow(
    clippy::struct_field_names,
    reason = "the selector preserves exact generated identity names across two typed query results"
)]
struct CompleteProjectionBinding {
    stage_run_id: StageRunId,
    product_session_id: ProductSessionId,
    worker_session_id: WorkerSessionId,
    codex_thread_id: CodexThreadId,
}

fn delivery_projection_result(
    response: &QueryResultResponse,
) -> Result<&DeliveryDetailProjection, String> {
    match response {
        QueryResultResponse::DeliveryGetResultResponse(response) => Ok(&response.result),
        _ => Err("delivery.get returned a different generated query-result branch".to_owned()),
    }
}

fn last_complete_projection_binding(
    stages: &[DeliveryStageProjection],
) -> Option<CompleteProjectionBinding> {
    stages.iter().rev().find_map(|stage| {
        if stage.actor_type != "codex" {
            return None;
        }
        let binding = stage.session_binding.as_ref()?;
        Some(CompleteProjectionBinding {
            stage_run_id: stage.id.clone(),
            product_session_id: binding.product_session_id.clone(),
            worker_session_id: binding.worker_session_id.clone()?,
            codex_thread_id: binding.codex_thread_id.clone()?,
        })
    })
}

fn validate_runtime_projection_result(
    response: &QueryResultResponse,
    binding: &CompleteProjectionBinding,
) -> Result<(), String> {
    let QueryResultResponse::RuntimeProjectionGetResultResponse(response) = response else {
        return Err(
            "runtime.projection.get returned a different generated query-result branch".to_owned(),
        );
    };
    let [session] = response.result.sessions.as_slice() else {
        return Err(format!(
            "runtime.projection.get must return exactly one exact session, got {}",
            response.result.sessions.len()
        ));
    };
    if response.result.stage_run_id.as_ref() != Some(&binding.stage_run_id)
        || response.result.product_session_id != binding.product_session_id
        || session.stage_run_id.as_ref() != Some(&binding.stage_run_id)
        || session.product_session_id != binding.product_session_id
        || session.worker_session_id != binding.worker_session_id
        || session.codex_thread_id != binding.codex_thread_id
    {
        return Err(
            "runtime.projection.get did not return the exact selected Delivery binding".to_owned(),
        );
    }
    Ok(())
}

fn strict_query_result(result: QueryResultResponse) -> Result<Value, String> {
    let value = serde_json::to_value(&result).map_err(string_error)?;
    let decoded: QueryResultResponse = serde_json::from_value(value.clone())
        .map_err(|error| format!("generated query response rejected: {error}"))?;
    serde_json::to_value(decoded).map_err(string_error)
}

fn fixture_scope(scenario: &str) -> Value {
    json!({
        "kind": "repository",
        "organizationId": canonical_id("org_", scenario),
        "workspaceId": canonical_id("wsp_", scenario),
        "projectId": canonical_id("prj_", scenario),
        "repositoryId": canonical_id("rep_", scenario),
    })
}

fn fixture_ownership(scenario: &str) -> Value {
    json!({
        "organizationId": canonical_id("org_", scenario),
        "workspaceId": canonical_id("wsp_", scenario),
        "projectId": canonical_id("prj_", scenario),
        "repositoryId": canonical_id("rep_", scenario),
    })
}

fn completed_response(
    command: &str,
    request_id: &str,
    previous_revision: u64,
    delivery: &Delivery,
    scenario: &str,
) -> Result<Value, String> {
    let request_id = canonical_request_id(request_id);
    let value = json!({
        "schemaVersion": API_SCHEMA,
        "command": command,
        "requestId": request_id,
        "previousRevision": previous_revision,
        "currentRevision": delivery.revision(),
        "outcome": "completed",
        "result": delivery_summary(delivery, scenario)?,
    });
    let response: CommandCompletedResponse = serde_json::from_value(value)
        .map_err(|error| format!("generated completed response rejected: {error}"))?;
    serde_json::to_value(response).map_err(string_error)
}

fn delivery_summary(delivery: &Delivery, scenario: &str) -> Result<Value, String> {
    let snapshot = delivery.snapshot();
    let mut task_counts = BTreeMap::from([
        ("pending", 0_u64),
        ("active", 0),
        ("blocked", 0),
        ("verifying", 0),
        ("completed", 0),
        ("failed", 0),
    ]);
    for task in &snapshot.tasks {
        let key = match task.status {
            winwincode_delivery::domain::DeliveryTaskStatus::Pending => "pending",
            winwincode_delivery::domain::DeliveryTaskStatus::Active => "active",
            winwincode_delivery::domain::DeliveryTaskStatus::Blocked => "blocked",
            winwincode_delivery::domain::DeliveryTaskStatus::Verifying => "verifying",
            winwincode_delivery::domain::DeliveryTaskStatus::Completed => "completed",
            winwincode_delivery::domain::DeliveryTaskStatus::Failed => "failed",
        };
        *task_counts.get_mut(key).expect("known task status") += 1;
    }
    let active = snapshot.stage_runs.iter().find(|run| {
        matches!(
            run.status,
            StageRunStatus::Running | StageRunStatus::Waiting
        )
    });
    Ok(json!({
        "schemaVersion": API_SCHEMA,
        "deliveryId": snapshot.id,
        "revision": snapshot.revision,
        "status": snapshot.status,
        "title": snapshot.spec.title,
        "updatedAt": millis_to_rfc3339(snapshot.updated_at_millis)?,
        "activeStageRunId": active.map(|run| &run.id),
        "openAttentionCount": snapshot.attention_items.iter().filter(|item| {
            item.status == winwincode_delivery::domain::AttentionItemStatus::Open
        }).count(),
        "taskCounts": {
            "total": snapshot.tasks.len(),
            "pending": task_counts["pending"],
            "active": task_counts["active"],
            "blocked": task_counts["blocked"],
            "verifying": task_counts["verifying"],
            "completed": task_counts["completed"],
            "failed": task_counts["failed"],
        },
        "ownership": fixture_ownership(scenario),
    }))
}

#[allow(
    clippy::too_many_lines,
    reason = "the observer returns one closed state, journal, receipt, and outbox snapshot"
)]
fn sqlite_durable_observation(home: &Path, delivery_id: &DeliveryId) -> Result<Value, String> {
    let connection = Connection::open_with_flags(
        home.join("control-plane.sqlite3"),
        OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_FULL_MUTEX,
    )
    .map_err(string_error)?;
    let stream_id = format!("delivery:{}", delivery_id.0);
    let state = connection
        .query_row(
            "SELECT revision, payload FROM product_state WHERE stream_id = ?1",
            [&stream_id],
            |row| Ok((row.get::<_, i64>(0)?, row.get::<_, Vec<u8>>(1)?)),
        )
        .optional()
        .map_err(string_error)?
        .map(|(revision, payload)| {
            Ok::<_, String>(json!({
                "streamId": stream_id,
                "revision": revision,
                "snapshot": decode_json_bytes(&payload)?,
            }))
        })
        .transpose()?
        .ok_or_else(|| format!("durable state {stream_id} is absent"))?;
    let manifest_bytes = connection
        .query_row(
            "SELECT manifest FROM aggregate_journals \
             WHERE aggregate_type = 'delivery' AND aggregate_id = ?1",
            [&delivery_id.0],
            |row| row.get::<_, Vec<u8>>(0),
        )
        .optional()
        .map_err(string_error)?
        .ok_or_else(|| "durable Delivery journal manifest is absent".to_owned())?;
    let manifest = DeliveryJournalCodec::decode_manifest(&manifest_bytes).map_err(string_error)?;
    let mut record_statement = connection
        .prepare(
            "SELECT payload FROM aggregate_journal_records \
             WHERE aggregate_type = 'delivery' AND aggregate_id = ?1 ORDER BY sequence",
        )
        .map_err(string_error)?;
    let records = record_statement
        .query_map([&delivery_id.0], |row| row.get::<_, Vec<u8>>(0))
        .map_err(string_error)?
        .map(|row| {
            let bytes = row.map_err(string_error)?;
            let record = DeliveryJournalCodec::decode_record(&bytes).map_err(string_error)?;
            Ok::<_, String>(json!({
                "schemaVersion": record.schema_version,
                "deliveryId": record.delivery_id,
                "sequence": record.sequence,
                "requestId": record.request_id,
                "requestDigest": record.request_digest,
                "operation": record.operation,
                "previousDigest": record.previous_digest,
                "snapshot": record.snapshot.snapshot(),
                "digest": record.digest,
            }))
        })
        .collect::<Result<Vec<_>, _>>()?;
    let journal_snapshot = records
        .last()
        .and_then(|record| record.get("snapshot"))
        .cloned()
        .ok_or_else(|| "durable Delivery journal has no records".to_owned())?;

    let mut receipt_statement = connection
        .prepare(
            "SELECT actor_key, scope_key, request_id, stream_id, revision \
             FROM command_receipts WHERE stream_id = ?1 ORDER BY rowid",
        )
        .map_err(string_error)?;
    let receipt_rows = receipt_statement
        .query_map([&stream_id], |row| {
            Ok((
                row.get::<_, Vec<u8>>(0)?,
                row.get::<_, Vec<u8>>(1)?,
                row.get::<_, String>(2)?,
                row.get::<_, String>(3)?,
                row.get::<_, i64>(4)?,
            ))
        })
        .map_err(string_error)?;
    let mut receipts = Vec::new();
    for row in receipt_rows {
        let (actor_key, scope_key, request_id, receipt_stream_id, revision) =
            row.map_err(string_error)?;
        let events =
            sqlite_receipt_events(&connection, &actor_key, &scope_key, &request_id, false)?;
        receipts.push(json!({
            "actorKey": lowercase_hex(&actor_key),
            "scopeKey": lowercase_hex(&scope_key),
            "requestId": request_id,
            "streamId": receipt_stream_id,
            "revision": revision,
            "idempotentReplay": false,
            "events": events,
        }));
    }
    let outbox = sqlite_all_outbox(&connection)?;
    Ok(json!({
        "state": state,
        "journal": {
            "manifest": manifest,
            "records": records,
            "snapshot": journal_snapshot,
        },
        "receipts": receipts,
        "outbox": outbox,
    }))
}

fn sqlite_receipt_events(
    connection: &Connection,
    actor_key: &[u8],
    scope_key: &[u8],
    request_id: &str,
    include_published: bool,
) -> Result<Vec<Value>, String> {
    let mut statement = connection
        .prepare(
            "SELECT sequence, event_id, topic, payload, projection_stream_kind, \
                    projection_resource_id, projection_stream_sequence, published \
             FROM outbox WHERE receipt_actor_key = ?1 AND receipt_scope_key = ?2 \
               AND request_id = ?3 ORDER BY sequence",
        )
        .map_err(string_error)?;
    statement
        .query_map(params![actor_key, scope_key, request_id], sqlite_outbox_row)
        .map_err(string_error)?
        .map(|row| {
            let row = row.map_err(string_error)?;
            Ok(if include_published {
                row
            } else {
                let mut row = row.as_object().expect("outbox row").clone();
                row.remove("published");
                Value::Object(row)
            })
        })
        .collect()
}

fn sqlite_all_outbox(connection: &Connection) -> Result<Vec<Value>, String> {
    let mut statement = connection
        .prepare(
            "SELECT sequence, event_id, topic, payload, projection_stream_kind, \
                    projection_resource_id, projection_stream_sequence, published \
             FROM outbox ORDER BY sequence",
        )
        .map_err(string_error)?;
    statement
        .query_map([], sqlite_outbox_row)
        .map_err(string_error)?
        .map(|row| row.map_err(string_error))
        .collect()
}

fn sqlite_outbox_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<Value> {
    let sequence = row.get::<_, i64>(0)?;
    let event_id = row.get::<_, String>(1)?;
    let topic = row.get::<_, String>(2)?;
    let payload = row.get::<_, Vec<u8>>(3)?;
    let stream_kind = row.get::<_, Option<String>>(4)?;
    let resource_id = row.get::<_, Option<String>>(5)?;
    let stream_sequence = row.get::<_, Option<i64>>(6)?;
    let published = row.get::<_, i64>(7)? != 0;
    let projection_cursor = match (stream_kind, resource_id, stream_sequence) {
        (Some(kind), Some(resource_id), Some(sequence)) => json!({
            "kind": kind,
            "resourceId": resource_id,
            "sequence": sequence,
            "eventId": event_id,
        }),
        (None, None, None) => Value::Null,
        _ => Value::String("invalid-stored-projection-cursor".to_owned()),
    };
    Ok(json!({
        "sequence": sequence,
        "eventId": event_id,
        "topic": topic,
        "payload": serde_json::from_slice::<Value>(&payload)
            .unwrap_or_else(|_| Value::String(lowercase_hex(&payload))),
        "projectionCursor": projection_cursor,
        "published": published,
    }))
}

fn decode_json_bytes(bytes: &[u8]) -> Result<Value, String> {
    serde_json::from_slice(bytes).map_err(string_error)
}

fn lowercase_hex(bytes: &[u8]) -> String {
    bytes.iter().fold(
        String::with_capacity(bytes.len().saturating_mul(2)),
        |mut output, byte| {
            write!(&mut output, "{byte:02x}").expect("writing hexadecimal bytes cannot fail");
            output
        },
    )
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct SeedRecordDigestInput<'a> {
    schema_version: u8,
    delivery_id: &'a DeliveryId,
    sequence: &'a str,
    request_id: &'a RequestId,
    request_digest: &'a str,
    operation: DeliveryMutationOperation,
    previous_digest: Option<&'a str>,
    snapshot: &'a Delivery,
}

#[allow(
    clippy::too_many_lines,
    reason = "the seed fixture commits one complete SQLite state, journal, receipt, and outbox chain"
)]
fn seed_snapshot_sqlite(home: &Path, scenario: &str, delivery: &Delivery) -> Result<(), String> {
    if delivery.revision() != 1 {
        return Err("seed-snapshot fixture requires a revision-1 Delivery".to_owned());
    }
    let request_id = RequestId(canonical_request_id(&format!("fixture:seed:{scenario}")));
    let request_digest = format!(
        "{:x}",
        Sha256::digest(delivery.encode_json().map_err(string_error)?)
    );
    let sequence = "1".to_owned();
    let digest_input = SeedRecordDigestInput {
        schema_version: DELIVERY_STORE_SCHEMA_VERSION,
        delivery_id: delivery.id(),
        sequence: &sequence,
        request_id: &request_id,
        request_digest: &request_digest,
        operation: DeliveryMutationOperation::DeliveryCreated,
        previous_digest: None,
        snapshot: delivery,
    };
    let digest = format!(
        "{:x}",
        Sha256::digest(serde_json::to_vec(&digest_input).map_err(string_error)?)
    );
    let record = DeliveryStoreRecord {
        schema_version: DELIVERY_STORE_SCHEMA_VERSION,
        delivery_id: delivery.id().clone(),
        sequence,
        request_id: request_id.clone(),
        request_digest,
        operation: DeliveryMutationOperation::DeliveryCreated,
        previous_digest: None,
        snapshot: delivery.clone(),
        digest: digest.clone(),
    };
    let manifest = DeliveryStoreManifest {
        schema_version: DELIVERY_STORE_SCHEMA_VERSION,
        delivery_id: delivery.id().clone(),
        created_at_millis: delivery.snapshot().created_at_millis,
        first_record_digest: digest.clone(),
    };
    let scope: RepositoryScope =
        serde_json::from_value(fixture_scope(scenario)).map_err(string_error)?;
    let scope_key = fixture_repository_scope_key(&scope)?;
    let actor_key =
        ReceiptActorKey::from_encoded(format!("fixture-seed-actor:{scenario}").into_bytes())
            .map_err(string_error)?;
    let receipt_identity =
        ReceiptIdentity::new(actor_key, scope_key.clone(), request_id).map_err(string_error)?;
    let event_payload = serde_json::to_vec(&ControlPlaneWebSocketDeliveryChangedEvent {
        change_kind: "created".to_owned(),
        delivery_id: delivery.id().clone(),
        revision: Revision(1),
        type_value: ControlPlaneWebSocketDeliveryChangedEventTypeValue::DeliveryChangedV1,
    })
    .map_err(string_error)?;
    let mut event_digest = Sha256::new();
    event_digest.update(b"winwincode.delivery-changed-event.v1\0");
    event_digest.update((scope_key.as_bytes().len() as u64).to_be_bytes());
    event_digest.update(scope_key.as_bytes());
    event_digest.update((event_payload.len() as u64).to_be_bytes());
    event_digest.update(&event_payload);
    let event_id =
        winwincode_domain::ControlPlaneEventId(format!("evt_{:x}", event_digest.finalize()));
    let publication = AggregateJournalPublication::Create {
        key: AggregateJournalKey::new("delivery", &delivery.id().0).map_err(string_error)?,
        manifest: DeliveryJournalCodec::encode_manifest(&manifest).map_err(string_error)?,
        first_record: AggregateJournalRecord::new(
            1,
            digest,
            DeliveryJournalCodec::encode_record(&record).map_err(string_error)?,
        ),
    };
    let command_digest = Sha256Digest(format!(
        "sha256:{:x}",
        Sha256::digest(serde_json::to_vec(&record).map_err(string_error)?)
    ));
    let mut storage = SqliteStorage::open(home).map_err(string_error)?;
    let receipt = storage
        .commit(
            &StateCommit::new(
                receipt_identity,
                command_digest,
                format!("delivery:{}", delivery.id().0),
                0,
                delivery.encode_json().map_err(string_error)?,
                vec![NewOutboxEvent::projection(
                    event_id,
                    "delivery.changed.v1",
                    event_payload,
                    ProjectionEventStream::Delivery(delivery.id().clone()),
                )],
            )
            .with_journal_publication(publication),
        )
        .map_err(string_error)?;
    for event in &receipt.events {
        storage
            .mark_published(&event.event_id)
            .map_err(string_error)?;
    }
    Box::new(storage).close().map_err(string_error)
}

fn fixture_repository_scope_key(scope: &RepositoryScope) -> Result<ReceiptScopeKey, String> {
    const PREFIX: &[u8] = b"winwincode.command-receipt.scope.v1";
    let mut encoded = Vec::new();
    for field in [
        PREFIX,
        b"repository".as_slice(),
        scope.organization_id.0.as_bytes(),
        scope.workspace_id.0.as_bytes(),
        scope.project_id.0.as_bytes(),
        scope.repository_id.0.as_bytes(),
    ] {
        encoded.extend_from_slice(&(field.len() as u64).to_be_bytes());
        encoded.extend_from_slice(field);
    }
    ReceiptScopeKey::from_encoded(encoded).map_err(string_error)
}

fn sqlite_journal_record_payload(
    home: &Path,
    delivery_id: &DeliveryId,
    sequence: u64,
) -> Result<Vec<u8>, String> {
    let connection = Connection::open_with_flags(
        home.join("control-plane.sqlite3"),
        OpenFlags::SQLITE_OPEN_READ_ONLY,
    )
    .map_err(string_error)?;
    connection
        .query_row(
            "SELECT payload FROM aggregate_journal_records \
             WHERE aggregate_type = 'delivery' AND aggregate_id = ?1 AND sequence = ?2",
            params![
                delivery_id.0,
                i64::try_from(sequence).map_err(string_error)?
            ],
            |row| row.get(0),
        )
        .map_err(string_error)
}

fn sqlite_replace_journal_record_payload(
    home: &Path,
    delivery_id: &DeliveryId,
    sequence: u64,
    payload: &[u8],
) -> Result<(), String> {
    let connection = Connection::open(home.join("control-plane.sqlite3")).map_err(string_error)?;
    let changed = connection
        .execute(
            "UPDATE aggregate_journal_records SET payload = ?1 \
             WHERE aggregate_type = 'delivery' AND aggregate_id = ?2 AND sequence = ?3",
            params![
                payload,
                delivery_id.0,
                i64::try_from(sequence).map_err(string_error)?
            ],
        )
        .map_err(string_error)?;
    if changed != 1 {
        return Err(format!(
            "controlled journal mutation expected one row, changed {changed}"
        ));
    }
    Ok(())
}

fn parse_execution_source(input: &Value) -> ExecutionSource {
    let runtime_events = input
        .get("runtimeEvents")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    ExecutionSource {
        candidate: input
            .get("candidate")
            .filter(|value| !value.is_null())
            .cloned(),
        candidate_fact: None,
        runtime_events,
    }
}

fn terminal_verifier_fact(payload: &Value) -> Result<TerminalVerifierFact, String> {
    let runtime_events = payload
        .get("runtimeEvents")
        .and_then(Value::as_array)
        .ok_or_else(|| "submitVerdict runtimeEvents must be an array".to_owned())?;
    let verifier_events = runtime_events
        .iter()
        .filter(|event| {
            event.get("kind").and_then(Value::as_str) == Some("turn.completed")
                && event.pointer("/source/roleId").and_then(Value::as_str) == Some("verifier")
                && event.get("terminalReason").and_then(Value::as_str) == Some("completed")
                && event.pointer("/data/error").is_some_and(Value::is_null)
        })
        .collect::<Vec<_>>();
    let [event] = verifier_events.as_slice() else {
        return Err(format!(
            "submitVerdict must carry exactly one verifier turn.completed fact, found {}",
            verifier_events.len()
        ));
    };
    let cursor = object(event, "cursor")?;
    let sequence = cursor
        .get("sequence")
        .and_then(|value| {
            value
                .as_u64()
                .or_else(|| value.as_str().and_then(|value| value.parse().ok()))
        })
        .ok_or_else(|| "terminal verifier cursor.sequence is invalid".to_owned())?;
    Ok(TerminalVerifierFact {
        key: TerminalVerifierFactKey {
            legacy_session_id: required_str(object(event, "source")?, "sessionId")?.to_owned(),
            last_event_sequence: sequence,
            occurred_at_millis: required_u64(event, "occurredAtMillis")?,
        },
        summary: required_str(object(event, "data")?, "last_agent_message")?.to_owned(),
    })
}

fn verification_semantic_authority(
    delivery: &Delivery,
    candidate: &FrozenDeliveryCandidate,
    legacy_candidate: &Value,
    legacy_spec_identity: &LegacySpecIdentity,
) -> Result<VerificationSemanticAuthority, String> {
    let spec = &delivery.snapshot().spec;
    if candidate.delivery_id() != delivery.id()
        || candidate.delivery_spec_id() != &spec.id
        || candidate.delivery_spec_revision() != spec.revision
    {
        return Err(
            "verification candidate does not name the current Delivery and DeliverySpec".to_owned(),
        );
    }
    let legacy_delivery_id = required_str(legacy_candidate, "deliveryId")?;
    let legacy_spec_id = required_str(legacy_candidate, "deliverySpecId")?;
    let legacy_spec_revision = required_u64(legacy_candidate, "deliverySpecRevision")?;
    if legacy_delivery_id != delivery.id().0.as_str()
        || legacy_spec_id != legacy_spec_identity.id.as_str()
        || legacy_spec_revision != legacy_spec_identity.revision
        || legacy_spec_revision != spec.revision
    {
        return Err(format!(
            "legacy verification candidate names Delivery {legacy_delivery_id} Spec {legacy_spec_id}@{legacy_spec_revision}, expected {} {}@{} mapped to {}@{}",
            delivery.id().0,
            legacy_spec_identity.id,
            legacy_spec_identity.revision,
            spec.id.0,
            spec.revision,
        ));
    }
    Ok(VerificationSemanticAuthority {
        identity: LegacyVerificationIdentity {
            candidate_ref: required_str(legacy_candidate, "candidateRef")?.to_owned(),
            delivery_spec_id: legacy_spec_identity.id.clone(),
            delivery_spec_revision: legacy_spec_identity.revision,
            criterion_ids: spec
                .acceptance_criteria
                .iter()
                .map(|criterion| criterion.id.0.clone())
                .collect(),
        },
        required_roles: ["reviewer".to_owned(), "verifier".to_owned()]
            .into_iter()
            .collect(),
    })
}

fn verdict_outcome_from_semantics(
    payload: &Value,
    authority: &VerificationSemanticAuthority,
) -> Result<VerdictFixtureOutcome, String> {
    let declared_roles = declared_verification_roles(payload, authority)?;
    let runtime_events = payload
        .get("runtimeEvents")
        .and_then(Value::as_array)
        .ok_or_else(|| "submitVerdict runtimeEvents must be an array".to_owned())?;
    let mut reviewer = None;
    let mut verifier = None;
    let mut shared_identity = None;
    for event in runtime_events.iter().filter(|event| {
        event.pointer("/semantic/kind").and_then(Value::as_str) == Some("verification-result")
    }) {
        let parsed = legacy_verification_result(event, &declared_roles, authority)?;
        if shared_identity
            .as_ref()
            .is_some_and(|shared| shared != &parsed.identity)
        {
            return Err(
                "Reviewer and Verifier results do not name the same candidate, Spec, and criteria"
                    .to_owned(),
            );
        }
        shared_identity.get_or_insert(parsed.identity);
        let target = match parsed.role.as_str() {
            "reviewer" => &mut reviewer,
            "verifier" => &mut verifier,
            other => {
                return Err(format!("verification-result has unsupported role {other}"));
            }
        };
        if target.replace(parsed.verdict).is_some() {
            return Err(format!(
                "submitVerdict repeats the {} verification-result",
                parsed.role
            ));
        }
    }
    let reviewer = reviewer.ok_or_else(|| "submitVerdict lacks Reviewer result".to_owned())?;
    let verifier = verifier.ok_or_else(|| "submitVerdict lacks Verifier result".to_owned())?;
    match (reviewer, verifier) {
        (LegacyVerificationVerdict::Pass, LegacyVerificationVerdict::Pass) => {
            Ok(VerdictFixtureOutcome::Pass)
        }
        (LegacyVerificationVerdict::Fail, LegacyVerificationVerdict::Fail) => {
            Ok(VerdictFixtureOutcome::Fail)
        }
        (LegacyVerificationVerdict::Pass, LegacyVerificationVerdict::Inconclusive) => {
            Ok(VerdictFixtureOutcome::Inconclusive)
        }
        (LegacyVerificationVerdict::InfraError, LegacyVerificationVerdict::InfraError) => {
            Ok(VerdictFixtureOutcome::InfraError)
        }
        pair => Err(format!(
            "submitVerdict has unsupported Reviewer/Verifier verdict pair {pair:?}"
        )),
    }
}

fn declared_verification_roles(
    payload: &Value,
    authority: &VerificationSemanticAuthority,
) -> Result<BTreeSet<String>, String> {
    let required_roles = payload
        .get("requiredRoles")
        .and_then(Value::as_array)
        .ok_or_else(|| "submitVerdict requiredRoles must be an array".to_owned())?;
    let mut declared_roles = BTreeSet::new();
    for role in required_roles {
        let role = role
            .as_str()
            .ok_or_else(|| "submitVerdict requiredRoles entries must be strings".to_owned())?;
        if !declared_roles.insert(role.to_owned()) {
            return Err(format!("submitVerdict repeats required role {role}"));
        }
    }
    if declared_roles != authority.required_roles {
        return Err(
            "submitVerdict requiredRoles do not match the sealed verification roles".to_owned(),
        );
    }
    Ok(declared_roles)
}

fn legacy_verification_result(
    event: &Value,
    declared_roles: &BTreeSet<String>,
    authority: &VerificationSemanticAuthority,
) -> Result<LegacyVerificationResult, String> {
    if event.get("kind").and_then(Value::as_str) != Some("message.completed") {
        return Err("verification-result semantic must come from message.completed".to_owned());
    }
    let semantic = object(event, "semantic")?;
    if required_str(semantic, "protocol")? != "winwincode.independent-verification-result.v1" {
        return Err("verification-result protocol is not canonical v1".to_owned());
    }
    let role = required_str(object(event, "source")?, "roleId")?.to_owned();
    if !declared_roles.contains(&role) {
        return Err(format!(
            "verification-result role {role} is not required by this verdict"
        ));
    }
    let findings = semantic
        .get("findings")
        .and_then(Value::as_array)
        .filter(|findings| !findings.is_empty())
        .ok_or_else(|| format!("{role} verification-result findings must be non-empty"))?;
    let mut criterion_ids = BTreeSet::new();
    let mut finding_verdict = None;
    for finding in findings {
        let criterion_id = required_str(finding, "criterionId")?.to_owned();
        if !criterion_ids.insert(criterion_id.clone()) {
            return Err(format!(
                "{role} verification-result repeats criterion {criterion_id}"
            ));
        }
        let verdict = legacy_verification_verdict(required_str(finding, "verdict")?)?;
        if finding_verdict.is_some_and(|current| current != verdict) {
            return Err(format!(
                "{role} verification-result mixes criterion verdicts"
            ));
        }
        finding_verdict = Some(verdict);
    }
    let identity = LegacyVerificationIdentity {
        candidate_ref: required_str(semantic, "candidateRef")?.to_owned(),
        delivery_spec_id: required_str(semantic, "deliverySpecId")?.to_owned(),
        delivery_spec_revision: required_u64(semantic, "deliverySpecRevision")?,
        criterion_ids,
    };
    validate_verification_identity(&role, &identity, authority)?;
    Ok(LegacyVerificationResult {
        role,
        verdict: finding_verdict.expect("non-empty findings"),
        identity,
    })
}

fn validate_verification_identity(
    role: &str,
    identity: &LegacyVerificationIdentity,
    authority: &VerificationSemanticAuthority,
) -> Result<(), String> {
    if identity.candidate_ref != authority.identity.candidate_ref {
        return Err(format!(
            "{role} verification-result names candidate {}, expected {}",
            identity.candidate_ref, authority.identity.candidate_ref
        ));
    }
    if identity.delivery_spec_id != authority.identity.delivery_spec_id
        || identity.delivery_spec_revision != authority.identity.delivery_spec_revision
    {
        return Err(format!(
            "{role} verification-result names a foreign DeliverySpec"
        ));
    }
    if identity.criterion_ids != authority.identity.criterion_ids {
        return Err(format!(
            "{role} verification-result does not cover every current acceptance criterion"
        ));
    }
    Ok(())
}

fn legacy_verification_verdict(value: &str) -> Result<LegacyVerificationVerdict, String> {
    match value {
        "pass" => Ok(LegacyVerificationVerdict::Pass),
        "fail" => Ok(LegacyVerificationVerdict::Fail),
        "inconclusive" => Ok(LegacyVerificationVerdict::Inconclusive),
        "infra_error" => Ok(LegacyVerificationVerdict::InfraError),
        other => Err(format!(
            "verification-result has unsupported verdict {other}"
        )),
    }
}

const fn delivery_terminal_outcome_status(
    status: &ExecutionOutcomeStatus,
) -> TerminalOutcomeStatus {
    match status {
        ExecutionOutcomeStatus::Succeeded => TerminalOutcomeStatus::Succeeded,
        ExecutionOutcomeStatus::InfrastructureError => TerminalOutcomeStatus::InfrastructureError,
        ExecutionOutcomeStatus::Failed => TerminalOutcomeStatus::Failed,
        ExecutionOutcomeStatus::Cancelled => TerminalOutcomeStatus::Cancelled,
    }
}

fn parse_execution_source_with_candidates(
    input: &Value,
    candidates: &HashMap<String, FrozenDeliveryCandidate>,
) -> ExecutionSource {
    let mut source = parse_execution_source(input);
    let Some(candidate_ref) = source
        .candidate
        .as_ref()
        .and_then(|candidate| candidate.get("candidateRef"))
        .and_then(Value::as_str)
    else {
        return source;
    };
    source.candidate_fact = candidates
        .get(candidate_ref)
        .or_else(|| {
            candidates
                .values()
                .find(|candidate| candidate.candidate_ref() == candidate_ref)
        })
        .cloned();
    source
}

fn migrate_legacy_snapshot(mut snapshot: Value) -> Result<Value, String> {
    let delivery_id = required_str(&snapshot, "id")?.to_owned();
    let task_id_map = snapshot
        .get("tasks")
        .and_then(Value::as_array)
        .ok_or_else(|| "legacy seed tasks must be an array".to_owned())?
        .iter()
        .map(|task| {
            let source = required_str(task, "id")?;
            let canonical = canonical_migrated_task_id(&delivery_id, source);
            Ok::<_, String>((source.to_owned(), canonical))
        })
        .collect::<Result<HashMap<_, _>, _>>()?;
    for task in snapshot
        .get_mut("tasks")
        .and_then(Value::as_array_mut)
        .expect("tasks checked above")
    {
        let source = required_str(task, "id")?.to_owned();
        task["id"] = Value::String(
            task_id_map
                .get(&source)
                .expect("task identity collected above")
                .clone(),
        );
        task["owner"] = Value::Null;
        let dependencies = task
            .get_mut("blockedByTaskIds")
            .and_then(Value::as_array_mut)
            .ok_or_else(|| format!("legacy seed task {source} lacks blockedByTaskIds"))?;
        for dependency in dependencies {
            let source_dependency = dependency
                .as_str()
                .ok_or_else(|| "legacy seed dependency must be a string".to_owned())?;
            *dependency = Value::String(
                task_id_map
                    .get(source_dependency)
                    .ok_or_else(|| {
                        format!("legacy seed task {source} references missing {source_dependency}")
                    })?
                    .clone(),
            );
        }
    }
    let stage_runs = snapshot
        .get_mut("stageRuns")
        .and_then(Value::as_array_mut)
        .ok_or_else(|| "legacy seed stageRuns must be an array".to_owned())?;
    for run in &mut *stage_runs {
        if let Some(source_task_id) = run.get("deliveryTaskId").and_then(Value::as_str) {
            run["deliveryTaskId"] = Value::String(
                task_id_map
                    .get(source_task_id)
                    .ok_or_else(|| {
                        format!("legacy seed StageRun references missing task {source_task_id}")
                    })?
                    .clone(),
            );
        }
    }
    let runs = stage_runs
        .iter()
        .map(|run| Ok::<_, String>((required_str(run, "id")?.to_owned(), run.clone())))
        .collect::<Result<HashMap<_, _>, _>>()?;
    let bindings = snapshot
        .get_mut("sessionBindings")
        .and_then(Value::as_array_mut)
        .ok_or_else(|| "legacy seed sessionBindings must be an array".to_owned())?;
    let mut canonical = Vec::new();
    for binding in bindings.iter() {
        let stage_run_id = required_str(binding, "stageRunId")?;
        let run = runs
            .get(stage_run_id)
            .ok_or_else(|| format!("legacy seed binding references missing {stage_run_id}"))?;
        if required_str(run, "actorType")? == "human" {
            continue;
        }
        let dsh = required_str(binding, "dshSessionId")?;
        let codex = required_str(binding, "codexSessionId")?;
        canonical.push(json!({
            "schemaVersion": binding["schemaVersion"],
            "id": binding["id"],
            "deliveryId": delivery_id,
            "deliveryTaskId": run.get("deliveryTaskId").cloned().unwrap_or(Value::Null),
            "stageRunId": stage_run_id,
            "productSessionId": canonical_id("psn_", dsh),
            "executionJobId": canonical_id("job_", &format!("{delivery_id}:{stage_run_id}")),
            "workerSessionId": canonical_id("wsn_", dsh),
            "codexThreadId": canonical_id("cdx_", codex),
            "boundAtMillis": binding["boundAtMillis"],
        }));
    }
    *bindings = canonical;
    Ok(snapshot)
}

fn visit_legacy_task_graph(
    id: &str,
    graph: &BTreeMap<String, Vec<String>>,
    visiting: &mut HashSet<String>,
    visited: &mut HashSet<String>,
) -> bool {
    if visited.contains(id) {
        return false;
    }
    if !visiting.insert(id.to_owned()) {
        return true;
    }
    if graph[id]
        .iter()
        .any(|dependency| visit_legacy_task_graph(dependency, graph, visiting, visited))
    {
        return true;
    }
    visiting.remove(id);
    visited.insert(id.to_owned());
    false
}

fn legacy_task_graph_has_cycle(tasks: &[Value]) -> Result<bool, String> {
    let mut graph = BTreeMap::new();
    for task in tasks {
        let id = required_str(task, "id")?.to_owned();
        let dependencies = task
            .get("blockedByTaskIds")
            .and_then(Value::as_array)
            .ok_or_else(|| format!("legacy task {id} blockedByTaskIds must be an array"))?
            .iter()
            .map(|dependency| {
                dependency
                    .as_str()
                    .filter(|dependency| !dependency.is_empty())
                    .map(str::to_owned)
                    .ok_or_else(|| format!("legacy task {id} has an invalid dependency"))
            })
            .collect::<Result<Vec<_>, _>>()?;
        if graph.insert(id.clone(), dependencies).is_some() {
            return Err(format!("legacy task graph repeats task {id}"));
        }
    }
    for (id, dependencies) in &graph {
        for dependency in dependencies {
            if !graph.contains_key(dependency) {
                return Err(format!(
                    "legacy task {id} depends on missing task {dependency}"
                ));
            }
        }
    }

    let mut visiting = HashSet::new();
    let mut visited = HashSet::new();
    Ok(graph
        .keys()
        .any(|id| visit_legacy_task_graph(id, &graph, &mut visiting, &mut visited)))
}

fn canonical_migrated_task_id(delivery_id: &str, legacy_task_id: &str) -> String {
    if is_canonical_task_id(legacy_task_id) {
        return legacy_task_id.to_owned();
    }
    let canonical =
        format!("winwincode.oracle-task-id-migration.v1\0{delivery_id}\0{legacy_task_id}");
    let digest = Sha256::digest(canonical.as_bytes());
    let mut bytes = [0_u8; 16];
    bytes.copy_from_slice(&digest[..16]);
    let mut value = u128::from_be_bytes(bytes);
    let mut encoded = [b'0'; 26];
    for byte in encoded.iter_mut().rev() {
        *byte = CROCKFORD_BASE32[(value & 31) as usize];
        value >>= 5;
    }
    format!(
        "dtk_{}",
        encoded.into_iter().map(char::from).collect::<String>()
    )
}

fn is_canonical_task_id(value: &str) -> bool {
    value.strip_prefix("dtk_").is_some_and(|suffix| {
        suffix.len() == 26 && suffix.bytes().all(|byte| CROCKFORD_BASE32.contains(&byte))
    })
}

fn require_requested_stage(
    payload: &Value,
    transition: &StageAdvanceResult,
) -> Result<(), StagePreparationError> {
    let run = transition
        .delivery
        .snapshot()
        .stage_runs
        .last()
        .ok_or_else(|| {
            StagePreparationError::InvalidRequest(
                "delivery.advance did not append a StageRun".to_owned(),
            )
        })?;
    let invalid = |error: String| StagePreparationError::InvalidRequest(error);
    let requested_stage = required_str(payload, "stage").map_err(invalid)?;
    let actual_stage = serde_json::to_value(run.stage)
        .map_err(string_error)
        .map_err(invalid)?;
    let actual_stage = actual_stage
        .as_str()
        .ok_or_else(|| invalid("stage did not serialize as text".to_owned()))?;
    let requested_actor = required_str(payload, "actorType").map_err(invalid)?;
    let actual_actor = serde_json::to_value(run.actor_type)
        .map_err(string_error)
        .map_err(invalid)?;
    let actual_actor = actual_actor
        .as_str()
        .ok_or_else(|| invalid("actor did not serialize as text".to_owned()))?;
    let requested_role = required_str(payload, "role").map_err(invalid)?;
    if requested_stage != actual_stage
        || requested_actor != actual_actor
        || requested_role != run.role
    {
        return Err(StagePreparationError::WrongState(format!(
            "canonical delivery.advance selected {actual_stage}/{actual_actor}/{} instead of {requested_stage}/{requested_actor}/{requested_role}",
            run.role
        )));
    }
    match (&transition.effect, run.actor_type) {
        (StageAdvanceEffect::Dispatch(_), StageRunActorType::Codex)
        | (StageAdvanceEffect::Review(_), StageRunActorType::Human) => Ok(()),
        _ => Err(StagePreparationError::InvalidRequest(
            "delivery.advance produced a mismatched sealed effect".into(),
        )),
    }
}

fn command_entry(source_index: usize, kind: &str, request: Value, response: Value) -> Value {
    json!({
        "sourceCommandIndexes": [source_index],
        "kind": kind,
        "request": request,
        "response": response,
    })
}

fn fixture_entry(source_index: usize, kind: &str, input: Value, response: Value) -> Value {
    command_entry(
        source_index,
        "fixture.command",
        json!({ "kind": kind, "input": input }),
        response,
    )
}

fn fixture_completed(result: Value) -> Value {
    json!({ "outcome": "completed", "result": result })
}

fn canonical_error_envelope(
    request_id: &str,
    code: &str,
    message: &str,
    retryable: bool,
    details: BTreeMap<String, winwincode_api::generated::ErrorDetailValue>,
) -> Result<Value, String> {
    let request_id = canonical_request_id(request_id);
    let value = json!({
        "schemaVersion": API_SCHEMA,
        "requestId": request_id,
        "error": {
            "code": code,
            "message": message,
            "retryable": retryable,
            "details": details,
        },
    });
    let envelope: ErrorEnvelope = serde_json::from_value(value)
        .map_err(|error| format!("generated ErrorEnvelope rejected: {error}"))?;
    serde_json::to_value(envelope).map_err(string_error)
}

#[allow(
    clippy::cast_precision_loss,
    reason = "the generated recursive ErrorDetailValue represents every JSON number as f64"
)]
fn revision_details(
    current_revision: Option<u64>,
) -> BTreeMap<String, winwincode_api::generated::ErrorDetailValue> {
    let mut details = BTreeMap::new();
    if let Some(revision) = current_revision {
        details.insert(
            "currentRevision".to_owned(),
            winwincode_api::generated::ErrorDetailValue::Variant3(revision as f64),
        );
    }
    details
}

fn delivery_command_error_response(
    request_id: &str,
    error: &winwincode_control_plane::DeliveryCommandCommitError,
    current_revision: Option<u64>,
) -> Result<Value, String> {
    let code = serde_json::to_value(error.public_code()).map_err(string_error)?;
    let code = code
        .as_str()
        .ok_or_else(|| "public Delivery command error code is not text".to_owned())?;
    let mut details = error.public_details();
    details.extend(revision_details(current_revision));
    canonical_error_envelope(
        request_id,
        code,
        &error.to_string(),
        error.retryable(),
        details,
    )
}

fn storage_error_contract(error: &winwincode_control_plane::StorageError) -> (&'static str, bool) {
    use winwincode_control_plane::StorageErrorKind;
    match error.kind() {
        StorageErrorKind::InvalidInput => ("INVALID_REQUEST", false),
        StorageErrorKind::RevisionConflict => ("REVISION_CONFLICT", false),
        StorageErrorKind::RequestConflict => ("IDEMPOTENCY_CONFLICT", false),
        StorageErrorKind::JournalNotFound => ("RESOURCE_NOT_FOUND", false),
        StorageErrorKind::EventCursorExpired => ("READ_CURSOR_EXPIRED", true),
        StorageErrorKind::RequestReplayMissing
        | StorageErrorKind::JournalAlreadyExists
        | StorageErrorKind::JournalConflict
        | StorageErrorKind::Adapter
        | StorageErrorKind::Closed => ("SERVICE_UNAVAILABLE", true),
    }
}

fn commit_error_response(
    request_id: &str,
    error: &winwincode_control_plane::CommitError,
    current_revision: Option<u64>,
) -> Result<Value, String> {
    match error {
        winwincode_control_plane::CommitError::Storage(source) => {
            let (code, retryable) = storage_error_contract(source);
            canonical_error_envelope(
                request_id,
                code,
                &source.to_string(),
                retryable,
                revision_details(current_revision),
            )
        }
        winwincode_control_plane::CommitError::PublicationPending { source, .. } => {
            canonical_error_envelope(
                request_id,
                "SERVICE_UNAVAILABLE",
                &source.to_string(),
                true,
                revision_details(current_revision),
            )
        }
    }
}

fn coordination_error_response(
    request_id: &str,
    error: &CoordinationError,
    current_revision: Option<u64>,
) -> Result<Value, String> {
    canonical_error_envelope(
        request_id,
        coordination_error_code(error.code()),
        error.message(),
        false,
        revision_details(current_revision),
    )
}

fn delivery_execution_error_response(
    request_id: &str,
    error: &DeliveryExecutionError,
    current_revision: Option<u64>,
) -> Result<Value, String> {
    match error {
        DeliveryExecutionError::Coordination(source) => {
            let code = coordination_error_code(source.code());
            canonical_error_envelope(
                request_id,
                code,
                source.message(),
                false,
                revision_details(current_revision),
            )
        }
        DeliveryExecutionError::InvalidEffect(message) => canonical_error_envelope(
            request_id,
            "INVALID_REQUEST",
            message,
            false,
            revision_details(current_revision),
        ),
        DeliveryExecutionError::Commit(source) => canonical_error_envelope(
            request_id,
            "SERVICE_UNAVAILABLE",
            &source.to_string(),
            true,
            revision_details(current_revision),
        ),
        DeliveryExecutionError::CommittedPayloadInvalid { message, .. } => {
            canonical_error_envelope(
                request_id,
                "INTERNAL_ERROR",
                message,
                false,
                revision_details(current_revision),
            )
        }
        DeliveryExecutionError::DispatchAfterCommit { source, .. }
        | DeliveryExecutionError::AcknowledgeAfterDispatch { source, .. }
        | DeliveryExecutionError::ProjectionPublicationAfterDispatch { source, .. } => {
            canonical_error_envelope(
                request_id,
                "SERVICE_UNAVAILABLE",
                &source.to_string(),
                true,
                revision_details(current_revision),
            )
        }
    }
}

fn coordination_error_code(code: CoordinationErrorCode) -> &'static str {
    match code {
        CoordinationErrorCode::InvalidRequest => "INVALID_REQUEST",
        CoordinationErrorCode::RevisionConflict => "REVISION_CONFLICT",
        CoordinationErrorCode::WrongState
        | CoordinationErrorCode::Conflict
        | CoordinationErrorCode::AttentionRequired
        | CoordinationErrorCode::BindingConflict
        | CoordinationErrorCode::StaleAttention => "WRONG_STATE",
    }
}

fn session_binding_error_response(
    message_id: &str,
    error: &winwincode_control_plane::DeliverySessionBindingCommitError,
    current_revision: Option<u64>,
) -> Result<Value, String> {
    let (code, retryable, message) = match error {
        winwincode_control_plane::DeliverySessionBindingCommitError::Storage(source)
        | winwincode_control_plane::DeliverySessionBindingCommitError::CodexThreadPhase {
            source,
            ..
        } => {
            let (code, retryable) = storage_error_contract(source);
            (code, retryable, error.to_string())
        }
        winwincode_control_plane::DeliverySessionBindingCommitError::PublicationPending {
            ..
        } => ("SERVICE_UNAVAILABLE", true, error.to_string()),
    };
    let envelope = canonical_error_envelope(
        message_id,
        code,
        &message,
        retryable,
        revision_details(current_revision),
    )?;
    Ok(json!({
        "messageId": message_id,
        "outcome": "rejected",
        "currentRevision": current_revision,
        "error": envelope["error"],
    }))
}

fn terminal_outcome_error_response(
    message_id: &str,
    error: &winwincode_control_plane::DeliveryTerminalOutcomeCommitError,
    current_revision: Option<u64>,
) -> Result<Value, String> {
    let (code, retryable, message) = match error {
        winwincode_control_plane::DeliveryTerminalOutcomeCommitError::Storage(source) => {
            let (code, retryable) = storage_error_contract(source);
            (code, retryable, error.to_string())
        }
        winwincode_control_plane::DeliveryTerminalOutcomeCommitError::PublicationPending {
            ..
        } => ("SERVICE_UNAVAILABLE", true, error.to_string()),
    };
    let envelope = canonical_error_envelope(
        message_id,
        code,
        &message,
        retryable,
        revision_details(current_revision),
    )?;
    Ok(json!({
        "messageId": message_id,
        "outcome": "rejected",
        "currentRevision": current_revision,
        "error": envelope["error"],
    }))
}

fn projection_error_response(
    request_id: &str,
    error: &winwincode_control_plane::strongflow_projection::StrongFlowProjectionError,
) -> Result<Value, String> {
    let code = serde_json::to_value(error.code()).map_err(string_error)?;
    let code = code
        .as_str()
        .ok_or_else(|| "projection error code is not text".to_owned())?;
    let retryable = matches!(
        code,
        "RATE_LIMITED"
            | "READ_CURSOR_EXPIRED"
            | "SERVICE_UNAVAILABLE"
            | "TRUSTED_FACTS_UNAVAILABLE"
    );
    canonical_error_envelope(
        request_id,
        code,
        error.message(),
        retryable,
        BTreeMap::new(),
    )
}

fn verdict_error_response(
    request_id: &str,
    error: &winwincode_control_plane::DeliveryVerdictCommitError,
    current_revision: Option<u64>,
) -> Result<Value, String> {
    match error {
        winwincode_control_plane::DeliveryVerdictCommitError::Coordination(source) => {
            let code = match source.code() {
                CoordinationErrorCode::Conflict | CoordinationErrorCode::StaleAttention => {
                    "CANDIDATE_STALE"
                }
                CoordinationErrorCode::InvalidRequest => "INVALID_REQUEST",
                CoordinationErrorCode::RevisionConflict => "REVISION_CONFLICT",
                CoordinationErrorCode::WrongState
                | CoordinationErrorCode::AttentionRequired
                | CoordinationErrorCode::BindingConflict => "WRONG_STATE",
            };
            canonical_error_envelope(
                request_id,
                code,
                source.message(),
                false,
                revision_details(current_revision),
            )
        }
        winwincode_control_plane::DeliveryVerdictCommitError::Storage(source) => {
            let (code, retryable) = storage_error_contract(source);
            canonical_error_envelope(
                request_id,
                code,
                &source.to_string(),
                retryable,
                revision_details(current_revision),
            )
        }
        winwincode_control_plane::DeliveryVerdictCommitError::PublicationPending {
            source, ..
        } => canonical_error_envelope(
            request_id,
            "SERVICE_UNAVAILABLE",
            &source.to_string(),
            true,
            revision_details(current_revision),
        ),
    }
}

fn receipt_json(receipt: &winwincode_control_plane::CommitReceipt) -> Result<Value, String> {
    Ok(json!({
        "actorKey": lowercase_hex(receipt.receipt_identity.actor_key().as_bytes()),
        "scopeKey": lowercase_hex(receipt.receipt_identity.scope_key().as_bytes()),
        "requestId": receipt.receipt_identity.request_id(),
        "streamId": receipt.stream_id,
        "revision": receipt.revision,
        "idempotentReplay": receipt.idempotent_replay,
        "events": receipt.events.iter().map(|event| {
            Ok(json!({
                "sequence": event.sequence,
                "eventId": event.event_id,
                "topic": event.topic,
                "payload": decode_json_bytes(&event.payload)
                    .unwrap_or_else(|_| Value::String(lowercase_hex(&event.payload))),
                "projectionCursor": event.projection_cursor.as_ref().map(|cursor| {
                    let (kind, resource_id) = match cursor.key().stream() {
                        ProjectionEventStream::Delivery(id) => ("delivery", id.0.as_str()),
                        ProjectionEventStream::ProductSession(id) => {
                            ("product-session", id.0.as_str())
                        }
                    };
                    json!({
                        "kind": kind,
                        "resourceId": resource_id,
                        "sequence": cursor.sequence(),
                        "eventId": cursor.event_id().map(|id| id.0.as_str()),
                    })
                }),
            }))
        }).collect::<Result<Vec<_>, String>>()?,
    }))
}

fn execution_config_for_transition(
    delivery: &Delivery,
    transition: &StageAdvanceResult,
    repository_id: &RepositoryId,
) -> Result<DeliveryExecutionConfig, String> {
    transition.validate_projection().map_err(string_error)?;
    let checkout_revision = match &transition.effect {
        StageAdvanceEffect::Dispatch(intent) | StageAdvanceEffect::Resume(intent) => {
            intent.rework_authorization().map_or_else(
                || delivery.snapshot().spec.base_revision.clone(),
                |authorization| {
                    authorization
                        .previous_candidate()
                        .candidate_commit_id()
                        .to_owned()
                },
            )
        }
        StageAdvanceEffect::Review(_) | StageAdvanceEffect::Clarify(_) => {
            return Err("execution config requires a dispatch transition".to_owned());
        }
    };
    let now = delivery.snapshot().updated_at_millis;
    Ok(DeliveryExecutionConfig {
        payload_digest: Sha256Digest(format!(
            "sha256:{:x}",
            Sha256::digest(delivery.encode_json().map_err(string_error)?)
        )),
        workspace: ExecutionWorkspace {
            checkout_revision,
            repository_id: repository_id.clone(),
            write_mode: ExecutionWorkspaceWriteMode::Candidate,
        },
        limits: ExecutionLimits {
            deadline_at: Instant(millis_to_rfc3339(now.saturating_add(3_600_000))?),
            max_artifact_bytes: 10_000_000,
            max_runtime_seconds: 3_600,
        },
    })
}

fn candidate_fixture_input(input: &Value) -> Result<CandidateFixtureInput, String> {
    let diff_sha256 = required_str(input, "diffSha256")?.to_owned();
    let changed_paths = input
        .get("changedPaths")
        .and_then(Value::as_array)
        .ok_or_else(|| "candidate.changedPaths must be an array".to_owned())?
        .iter()
        .map(|path| {
            let state = match required_str(path, "state")? {
                "present" => CandidatePathState::Present,
                "deleted" => CandidatePathState::Deleted,
                value => return Err(format!("unknown candidate path state {value}")),
            };
            Ok(CandidatePathFact {
                path: required_str(path, "path")?.to_owned(),
                state,
                object_id: path
                    .get("objectId")
                    .and_then(Value::as_str)
                    .map(str::to_owned),
            })
        })
        .collect::<Result<Vec<_>, String>>()?;
    let changed_hunks = changed_paths
        .iter()
        .map(|path| CandidateHunkFact {
            file_path: path.path.clone(),
            hunk_sha256: format!(
                "{:x}",
                Sha256::digest(
                    format!(
                        "winwincode.oracle-candidate-hunk.v1\0{diff_sha256}\0{}",
                        path.path
                    )
                    .as_bytes()
                )
            ),
            source_hunk_sha256: None,
        })
        .collect();
    let candidate_commit_id = required_str(input, "candidateCommitId")?.to_owned();
    Ok(CandidateFixtureInput {
        base_commit_id: required_str(input, "baseCommitId")?.to_owned(),
        base_tree_id: required_str(input, "baseTreeId")?.to_owned(),
        candidate_commit_id: candidate_commit_id.clone(),
        candidate_tree_id: required_str(input, "candidateTreeId")?.to_owned(),
        diff_sha256: diff_sha256.clone(),
        changed_paths,
        changed_hunks,
        artifact_ref: format!("artifact:oracle:{candidate_commit_id}"),
        artifact_digest: Sha256Digest(format!("sha256:{diff_sha256}")),
        terminal_event_sequence: 1,
    })
}

fn canonical_rework_candidate_input(
    mut input: CandidateFixtureInput,
    authorization: &ReworkAuthorization,
) -> Result<CandidateFixtureInput, String> {
    authorization
        .previous_candidate()
        .candidate_commit_id()
        .clone_into(&mut input.base_commit_id);
    authorization
        .previous_candidate()
        .candidate_tree_id()
        .clone_into(&mut input.base_tree_id);
    for path in &input.changed_paths {
        if !authorization
            .targets()
            .iter()
            .any(|target| target.file_path() == path.path)
        {
            return Err(format!(
                "rework candidate path {} is outside the sealed authorization",
                path.path
            ));
        }
    }
    for hunk in &mut input.changed_hunks {
        let target = authorization
            .targets()
            .iter()
            .find(|target| target.file_path() == hunk.file_path)
            .ok_or_else(|| {
                format!(
                    "rework candidate hunk {} is outside the sealed authorization",
                    hunk.file_path
                )
            })?;
        hunk.source_hunk_sha256 = Some(target.hunk_sha256().to_owned());
    }
    Ok(input)
}

fn sequence(input: &Value) -> Result<u64, String> {
    input
        .get("sequence")
        .and_then(|value| {
            value
                .as_u64()
                .or_else(|| value.as_str().and_then(|value| value.parse().ok()))
        })
        .ok_or_else(|| "fixture record sequence is invalid".to_owned())
}

fn object<'value>(value: &'value Value, field: &str) -> Result<&'value Value, String> {
    value
        .get(field)
        .filter(|value| value.is_object())
        .ok_or_else(|| format!("{field} must be an object"))
}

fn required_str<'value>(value: &'value Value, field: &str) -> Result<&'value str, String> {
    value
        .get(field)
        .and_then(Value::as_str)
        .ok_or_else(|| format!("{field} must be a string"))
}

fn required_u64(value: &Value, field: &str) -> Result<u64, String> {
    value
        .get(field)
        .and_then(Value::as_u64)
        .ok_or_else(|| format!("{field} must be a non-negative integer"))
}

fn canonical_id(prefix: &str, input: &str) -> String {
    let digest = format!("{:X}", Sha256::digest(input.as_bytes()));
    format!("{prefix}{}", &digest[..26])
}

fn canonical_request_id(input: &str) -> String {
    if input.len() == 30
        && input.starts_with("req_")
        && input[4..]
            .bytes()
            .all(|byte| byte.is_ascii_uppercase() || byte.is_ascii_digit())
    {
        input.to_owned()
    } else {
        canonical_id("req_", input)
    }
}

fn canonical_stage_run_id(input: &str) -> StageRunId {
    if input.len() == 30 && input.starts_with("run_") {
        StageRunId(input.to_owned())
    } else {
        StageRunId(canonical_id("run_", input))
    }
}

fn canonical_session_binding_id(input: &str) -> Result<SessionBindingId, String> {
    let identity = input
        .strip_prefix("stage-")
        .or_else(|| input.strip_prefix("binding-"))
        .unwrap_or(input);
    SessionBindingId::new(canonical_id("sbn_", identity)).map_err(string_error)
}

fn canonical_attention_item_id(input: &str) -> AttentionItemId {
    if input.len() == 30 && input.starts_with("att_") {
        AttentionItemId(input.to_owned())
    } else {
        AttentionItemId(canonical_id("att_", input))
    }
}

fn safe_component(value: &str) -> Result<(), String> {
    if !value.is_empty()
        && value.len() <= 200
        && value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_'))
    {
        Ok(())
    } else {
        Err("scenario id is not a safe path component".into())
    }
}

fn millis_to_rfc3339(value: u64) -> Result<String, String> {
    let seconds = value / 1_000;
    let millis = value % 1_000;
    let days = i64::try_from(seconds / 86_400).map_err(string_error)?;
    let seconds_of_day = seconds % 86_400;
    let (year, month, day) = civil_from_days(days);
    if !(1970..=9999).contains(&year) {
        return Err("timestamp exceeds RFC 3339".into());
    }
    let hour = seconds_of_day / 3_600;
    let minute = (seconds_of_day % 3_600) / 60;
    let second = seconds_of_day % 60;
    Ok(if millis == 0 {
        format!("{year:04}-{month:02}-{day:02}T{hour:02}:{minute:02}:{second:02}Z")
    } else {
        format!("{year:04}-{month:02}-{day:02}T{hour:02}:{minute:02}:{second:02}.{millis:03}Z")
    })
}

fn civil_from_days(days_since_epoch: i64) -> (i64, i64, i64) {
    let z = days_since_epoch + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1_460 + doe / 36_524 - doe / 146_096) / 365;
    let mut year = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = doy - (153 * mp + 2) / 5 + 1;
    let month = mp + if mp < 10 { 3 } else { -9 };
    year += i64::from(month <= 2);
    (year, month, day)
}

fn io_error(error: std::io::Error) -> String {
    error.to_string()
}

fn string_error(error: impl std::fmt::Display) -> String {
    error.to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn seed_migration_rekeys_legacy_task_graph_for_generated_execution_scope() {
        let migrated = migrate_legacy_snapshot(json!({
            "schemaVersion": 3,
            "id": "dlv_5QNEEJDDVR7MC02RM22SXW5TMJ",
            "revision": 1,
            "createdAtMillis": 1,
            "updatedAtMillis": 1,
            "status": "executing",
            "spec": {},
            "tasks": [
                {
                    "id": "oracle-task-prerequisite",
                    "blockedByTaskIds": []
                },
                {
                    "id": "oracle-task-dependent",
                    "blockedByTaskIds": ["oracle-task-prerequisite"]
                }
            ],
            "stageRuns": [],
            "sessionBindings": [],
            "attentionItems": [],
            "evidence": [],
            "verdict": null
        }))
        .expect("migrate seed");

        let tasks = migrated["tasks"].as_array().expect("tasks");
        let prerequisite = tasks[0]["id"].as_str().expect("prerequisite id");
        let dependent = tasks[1]["id"].as_str().expect("dependent id");
        assert!(prerequisite.starts_with("dtk_"));
        assert!(dependent.starts_with("dtk_"));
        assert_ne!(prerequisite, dependent);
        assert_eq!(tasks[1]["blockedByTaskIds"], json!([prerequisite]));
    }

    #[test]
    fn task_id_migration_rekeys_prefix_shaped_non_crockford_identity() {
        let malformed = "dtk_IIIIIIIIIIIIIIIIIIIIIIIIII";
        let migrated = canonical_migrated_task_id("dlv_5QNEEJDDVR7MC02RM22SXW5TMJ", malformed);
        assert_ne!(migrated, malformed);
        assert!(is_canonical_task_id(&migrated));
    }

    #[test]
    fn runtime_binding_selector_uses_typed_projection_stage_order() {
        fn stage(suffix: &str) -> DeliveryStageProjection {
            DeliveryStageProjection {
                actor_type: "codex".to_owned(),
                attempt: 1,
                delivery_task_id: None,
                finished_at: None,
                id: StageRunId(format!("run_{suffix}")),
                role: "executor".to_owned(),
                session_binding: Some(DeliveryStageSessionBindingProjection {
                    binding_id: format!("sbn_{suffix}"),
                    bound_at: Instant("2061-11-23T19:33:20.100Z".to_owned()),
                    codex_thread_id: Some(CodexThreadId(format!("cdx_{suffix}"))),
                    execution_job_id: ExecutionJobId(format!("job_{suffix}")),
                    product_session_id: ProductSessionId(format!("psn_{suffix}")),
                    worker_session_id: Some(WorkerSessionId(format!("wsn_{suffix}"))),
                }),
                stage: "executing".to_owned(),
                started_at: Instant("2061-11-23T19:33:20.100Z".to_owned()),
                status: "running".to_owned(),
            }
        }

        let first = stage("00000000000000000000000001");
        let second = stage("00000000000000000000000002");
        let selected = last_complete_projection_binding(&[first.clone(), second.clone()])
            .expect("complete binding");
        assert_eq!(selected.stage_run_id.0, "run_00000000000000000000000002");

        let perturbed =
            last_complete_projection_binding(&[second, first]).expect("complete binding");
        assert_eq!(perturbed.stage_run_id.0, "run_00000000000000000000000001");
    }

    #[test]
    fn execution_port_count_includes_each_distinct_terminal_verifier_fact_once() {
        let oracle: Value = serde_json::from_slice(include_bytes!(
            "../../../../tests/fixtures/oracles/delivery-strongflow-typescript.v1.json"
        ))
        .expect("oracle");
        let source = oracle["scenarios"]
            .as_array()
            .expect("scenarios")
            .iter()
            .find(|scenario| scenario["id"] == "candidate-invalidation")
            .expect("candidate-invalidation");
        let commands = source["commands"]
            .as_array()
            .expect("commands")
            .iter()
            .map(|command| {
                let mut projected = serde_json::Map::new();
                projected.insert("kind".to_owned(), command["kind"].clone());
                if let Some(request) = command.get("request") {
                    projected.insert("request".to_owned(), request.clone());
                }
                if let Some(input) = command.get("input") {
                    projected.insert("input".to_owned(), input.clone());
                }
                serde_json::from_value(Value::Object(projected)).expect("plan command")
            })
            .collect();
        let mut scenario = ScenarioPlan {
            id: "candidate-invalidation".to_owned(),
            commands,
            terminal_outcome_status_by_source_command_index: BTreeMap::from([
                (17, ExecutionOutcomeStatus::Succeeded),
                (25, ExecutionOutcomeStatus::Succeeded),
            ]),
        };

        scenario
            .validate_terminal_outcome_statuses()
            .expect("closed terminal status map");
        assert_eq!(scenario.execution_port_message_count(), 10);
        let removed = scenario
            .terminal_outcome_status_by_source_command_index
            .remove(&25)
            .expect("source 25 status");
        assert!(scenario.validate_terminal_outcome_statuses().is_err());
        scenario
            .terminal_outcome_status_by_source_command_index
            .insert(25, removed);
        scenario
            .terminal_outcome_status_by_source_command_index
            .insert(17, ExecutionOutcomeStatus::Failed);
        assert!(scenario.validate_terminal_outcome_statuses().is_err());
    }

    #[test]
    fn embedded_plan_freezes_infrastructure_outcome_as_the_generated_enum_wire_value() {
        let oracle: Value = serde_json::from_slice(include_bytes!(
            "../../../../tests/fixtures/oracles/delivery-strongflow-typescript.v1.json"
        ))
        .expect("oracle");
        let scenario = oracle["scenarios"]
            .as_array()
            .expect("scenarios")
            .iter()
            .find(|scenario| scenario["id"] == "infra-error")
            .expect("infra-error");

        assert_eq!(
            local_fixture_terminal_outcome_statuses(scenario).expect("closed outcome map"),
            json!({ "17": "infrastructure_error" })
        );
    }

    #[test]
    fn embedded_plan_uses_the_unique_completed_submit_response_not_runtime_findings() {
        let oracle: Value = serde_json::from_slice(include_bytes!(
            "../../../../tests/fixtures/oracles/delivery-strongflow-typescript.v1.json"
        ))
        .expect("oracle");
        let mut scenario = oracle["scenarios"]
            .as_array()
            .expect("scenarios")
            .iter()
            .find(|scenario| scenario["id"] == "success-closed-loop")
            .expect("success")
            .clone();
        let runtime_events = scenario
            .pointer_mut("/commands/17/request/payload/runtimeEvents")
            .and_then(Value::as_array_mut)
            .expect("runtime events");
        let verifier_result = runtime_events
            .iter_mut()
            .find(|event| {
                event.pointer("/source/roleId").and_then(Value::as_str) == Some("verifier")
                    && event.pointer("/semantic/kind").and_then(Value::as_str)
                        == Some("verification-result")
            })
            .expect("verifier result");
        for finding in verifier_result
            .pointer_mut("/semantic/findings")
            .and_then(Value::as_array_mut)
            .expect("findings")
        {
            finding["verdict"] = json!("infra_error");
        }

        assert_eq!(
            local_fixture_terminal_outcome_statuses(&scenario).expect("closed outcome map"),
            json!({ "17": "succeeded" })
        );

        let final_run = scenario
            .pointer_mut("/commands/17/response/result/delivery/stageRuns")
            .and_then(Value::as_array_mut)
            .and_then(|runs| runs.last_mut())
            .expect("final verifier");
        final_run["status"] = json!("failed");
        assert!(local_fixture_terminal_outcome_statuses(&scenario).is_err());
    }

    fn failed_verdict_payload_and_authority() -> (Value, VerificationSemanticAuthority) {
        let oracle: Value = serde_json::from_slice(include_bytes!(
            "../../../../tests/fixtures/oracles/delivery-strongflow-typescript.v1.json"
        ))
        .expect("oracle");
        let scenario = oracle["scenarios"]
            .as_array()
            .expect("scenarios")
            .iter()
            .find(|scenario| scenario["id"] == "candidate-invalidation")
            .expect("candidate-invalidation");
        let payload = scenario
            .pointer("/commands/17/request/payload")
            .expect("failed verdict payload")
            .clone();
        let candidate = object(&payload, "candidate").expect("candidate");
        let current_spec = scenario
            .pointer("/commands/2/request/payload/spec")
            .expect("approved Spec");
        let authority = VerificationSemanticAuthority {
            identity: LegacyVerificationIdentity {
                candidate_ref: required_str(candidate, "candidateRef")
                    .expect("candidate ref")
                    .to_owned(),
                delivery_spec_id: required_str(current_spec, "id")
                    .expect("Spec id")
                    .to_owned(),
                delivery_spec_revision: required_u64(current_spec, "revision")
                    .expect("Spec revision"),
                criterion_ids: current_spec["acceptanceCriteria"]
                    .as_array()
                    .expect("criteria")
                    .iter()
                    .map(|criterion| {
                        required_str(criterion, "id")
                            .expect("criterion id")
                            .to_owned()
                    })
                    .collect(),
            },
            required_roles: ["reviewer".to_owned(), "verifier".to_owned()]
                .into_iter()
                .collect(),
        };
        (payload, authority)
    }

    fn mutate_verification_semantics(payload: &mut Value, mut mutate: impl FnMut(&mut Value)) {
        for event in payload["runtimeEvents"]
            .as_array_mut()
            .expect("runtime events")
            .iter_mut()
            .filter(|event| {
                event.pointer("/semantic/kind").and_then(Value::as_str)
                    == Some("verification-result")
            })
        {
            mutate(&mut event["semantic"]);
        }
    }

    #[test]
    fn verdict_semantics_are_bound_to_the_exact_candidate_spec_and_roles() {
        let (payload, authority) = failed_verdict_payload_and_authority();
        assert!(matches!(
            verdict_outcome_from_semantics(&payload, &authority).expect("failed semantics"),
            VerdictFixtureOutcome::Fail
        ));

        let mut foreign_candidate = payload.clone();
        mutate_verification_semantics(&mut foreign_candidate, |semantic| {
            semantic["candidateRef"] = json!(
                "git-candidate:sha256:ffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffff"
            );
        });
        assert!(verdict_outcome_from_semantics(&foreign_candidate, &authority).is_err());

        let mut wrong_spec_revision = payload.clone();
        mutate_verification_semantics(&mut wrong_spec_revision, |semantic| {
            semantic["deliverySpecRevision"] = json!(3);
        });
        assert!(verdict_outcome_from_semantics(&wrong_spec_revision, &authority).is_err());

        let mut wrong_spec_id = payload.clone();
        mutate_verification_semantics(&mut wrong_spec_id, |semantic| {
            semantic["deliverySpecId"] = json!("foreign-spec");
        });
        assert!(verdict_outcome_from_semantics(&wrong_spec_id, &authority).is_err());

        let mut extra_required_role = payload;
        extra_required_role["requiredRoles"] =
            json!(["reviewer", "verifier", "adversarial-verifier"]);
        assert!(verdict_outcome_from_semantics(&extra_required_role, &authority).is_err());
    }

    #[test]
    fn verdict_semantics_cover_every_current_acceptance_criterion() {
        let (mut payload, mut authority) = failed_verdict_payload_and_authority();
        authority
            .identity
            .criterion_ids
            .insert("criterion-current-second".to_owned());
        mutate_verification_semantics(&mut payload, |semantic| {
            let findings = semantic["findings"].as_array_mut().expect("findings");
            let mut second = findings[0].clone();
            second["criterionId"] = json!("criterion-current-second");
            second["findingId"] = json!("finding-current-second");
            findings.push(second);
        });
        assert!(matches!(
            verdict_outcome_from_semantics(&payload, &authority)
                .expect("complete criterion semantics"),
            VerdictFixtureOutcome::Fail
        ));

        mutate_verification_semantics(&mut payload, |semantic| {
            semantic["findings"].as_array_mut().expect("findings").pop();
        });
        assert!(verdict_outcome_from_semantics(&payload, &authority).is_err());
    }

    #[test]
    fn verdict_outcome_changes_only_with_the_closed_reviewer_verifier_pair() {
        let (mut payload, authority) = failed_verdict_payload_and_authority();
        let verifier = payload["runtimeEvents"]
            .as_array_mut()
            .expect("runtime events")
            .iter_mut()
            .find(|event| {
                event.pointer("/source/roleId").and_then(Value::as_str) == Some("verifier")
                    && event.pointer("/semantic/kind").and_then(Value::as_str)
                        == Some("verification-result")
            })
            .expect("Verifier result");
        verifier["semantic"]["findings"][0]["verdict"] = json!("pass");
        assert!(verdict_outcome_from_semantics(&payload, &authority).is_err());

        let reviewer = payload["runtimeEvents"]
            .as_array_mut()
            .expect("runtime events")
            .iter_mut()
            .find(|event| {
                event.pointer("/source/roleId").and_then(Value::as_str) == Some("reviewer")
                    && event.pointer("/semantic/kind").and_then(Value::as_str)
                        == Some("verification-result")
            })
            .expect("Reviewer result");
        reviewer["semantic"]["findings"][0]["verdict"] = json!("pass");
        assert!(matches!(
            verdict_outcome_from_semantics(&payload, &authority).expect("pass semantics"),
            VerdictFixtureOutcome::Pass
        ));

        let reviewer = payload["runtimeEvents"]
            .as_array_mut()
            .expect("runtime events")
            .iter_mut()
            .find(|event| {
                event.pointer("/source/roleId").and_then(Value::as_str) == Some("reviewer")
                    && event.pointer("/semantic/kind").and_then(Value::as_str)
                        == Some("verification-result")
            })
            .expect("Reviewer result");
        reviewer["semantic"]["findings"][0]["verdict"] = json!("unknown");
        assert!(verdict_outcome_from_semantics(&payload, &authority).is_err());
    }

    #[test]
    fn rework_dispatch_checks_out_the_exact_failed_candidate_commit() {
        let fixture = winwincode_delivery::domain::rework::test_support::authorized_rework_dispatch(
            &DeliveryId("dlv_5F602BP1WZ9D57773Y51JX5QVC".to_owned()),
        );

        let config = execution_config_for_transition(
            &fixture.source_delivery,
            &fixture.transition,
            &RepositoryId("repo_5F602BP1WZ9D57773Y51JX5QVC".to_owned()),
        )
        .expect("execution config");

        assert_eq!(
            config.workspace.checkout_revision,
            fixture.source_candidate_commit_id
        );
    }

    #[test]
    fn rework_candidate_migration_binds_the_authorized_old_to_new_delta() {
        let fixture = winwincode_delivery::domain::rework::test_support::authorized_rework_dispatch(
            &DeliveryId("dlv_5F602BP1WZ9D57773Y51JX5QVC".to_owned()),
        );
        let StageAdvanceEffect::Dispatch(intent) = &fixture.transition.effect else {
            panic!("authorized rework must dispatch");
        };
        let authorization = intent
            .rework_authorization()
            .expect("sealed rework authorization");
        let target = &authorization.targets()[0];
        let migrated = canonical_rework_candidate_input(
            CandidateFixtureInput {
                base_commit_id: "0".repeat(40),
                base_tree_id: "1".repeat(40),
                candidate_commit_id: "2".repeat(40),
                candidate_tree_id: "3".repeat(40),
                diff_sha256: "4".repeat(64),
                changed_paths: vec![CandidatePathFact {
                    path: target.file_path().to_owned(),
                    state: CandidatePathState::Present,
                    object_id: Some("5".repeat(40)),
                }],
                changed_hunks: vec![CandidateHunkFact {
                    file_path: target.file_path().to_owned(),
                    hunk_sha256: "6".repeat(64),
                    source_hunk_sha256: None,
                }],
                artifact_ref: "artifact:oracle:replacement".to_owned(),
                artifact_digest: Sha256Digest(format!("sha256:{}", "7".repeat(64))),
                terminal_event_sequence: 1,
            },
            authorization,
        )
        .expect("rework candidate migration");

        assert_eq!(
            migrated.base_commit_id,
            authorization.previous_candidate().candidate_commit_id()
        );
        assert_eq!(
            migrated.base_tree_id,
            authorization.previous_candidate().candidate_tree_id()
        );
        assert_eq!(
            migrated.changed_hunks[0].source_hunk_sha256.as_deref(),
            Some(target.hunk_sha256())
        );
    }

    #[test]
    fn execution_source_replacement_reuses_the_matching_sealed_candidate() {
        let fixture = winwincode_delivery::domain::rework::test_support::authorized_rework_dispatch(
            &DeliveryId("dlv_5F602BP1WZ9D57773Y51JX5QVC".to_owned()),
        );
        let StageAdvanceEffect::Dispatch(intent) = &fixture.transition.effect else {
            panic!("authorized rework must dispatch");
        };
        let candidate = intent
            .rework_authorization()
            .expect("sealed rework authorization")
            .previous_candidate()
            .clone();
        let candidates = HashMap::from([("legacy-candidate-ref".to_owned(), candidate.clone())]);

        let source = parse_execution_source_with_candidates(
            &json!({
                "candidate": { "candidateRef": "legacy-candidate-ref" },
                "runtimeEvents": [],
            }),
            &candidates,
        );

        assert_eq!(
            source
                .candidate_fact
                .as_ref()
                .expect("sealed candidate")
                .candidate_ref(),
            candidate.candidate_ref()
        );
    }

    #[test]
    #[allow(
        clippy::too_many_lines,
        reason = "the focused probe keeps all five verdict transcript checkpoints visible"
    )]
    fn verdict_scenarios_commit_each_distinct_terminal_fact_before_the_verdict() {
        let oracle: Value = serde_json::from_slice(include_bytes!(
            "../../../../tests/fixtures/oracles/delivery-strongflow-typescript.v1.json"
        ))
        .expect("oracle");
        let ids = [
            "success-closed-loop",
            "candidate-invalidation",
            "inconclusive",
            "infra-error",
            "rework",
        ];
        let root = std::env::temp_dir().join(format!(
            "winwincode-verdict-focused-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .expect("clock")
                .as_nanos()
        ));
        let scenarios = oracle["scenarios"]
            .as_array()
            .expect("scenarios")
            .iter()
            .filter(|scenario| scenario["id"].as_str().is_some_and(|id| ids.contains(&id)))
            .map(|scenario| {
                let commands = scenario["commands"]
                    .as_array()
                    .expect("commands")
                    .iter()
                    .map(|command| {
                        let mut projected = serde_json::Map::new();
                        projected.insert("kind".to_owned(), command["kind"].clone());
                        if let Some(request) = command.get("request") {
                            projected.insert("request".to_owned(), request.clone());
                        }
                        if let Some(input) = command.get("input") {
                            projected.insert("input".to_owned(), input.clone());
                        }
                        Value::Object(projected)
                    })
                    .collect::<Vec<_>>();
                json!({
                    "id": format!(
                        "renamed-{}",
                        scenario["id"].as_str().expect("scenario id")
                    ),
                    "commands": commands,
                    "terminalOutcomeStatusBySourceCommandIndex":
                        local_fixture_terminal_outcome_statuses(scenario)
                            .expect("terminal outcome plan facts"),
                })
            })
            .collect::<Vec<_>>();
        let serialized = serde_json::to_string(&json!({
            "schemaVersion": PLAN_SCHEMA,
            "oracleSchemaVersion": oracle["schemaVersion"],
            "bindings": {
                "ORACLE_ROOT": &root,
                "NODE_EXECUTABLE": "/usr/bin/node",
                "AUTH_PROOF": "verdict-focused-proof",
                "fixtureRandomIdentities": {}
            },
            "scenarios": scenarios
        }))
        .expect("plan JSON")
        .replace("<ORACLE_ROOT>", &root.to_string_lossy())
        .replace("<NODE_EXECUTABLE>", "/usr/bin/node")
        .replace("<AUTH_PROOF>", "verdict-focused-proof");
        let plan: DifferentialPlan = serde_json::from_str(&serialized).expect("plan");

        let result = run_differential_plan(&plan).expect("verdict execution");
        let scenarios = result["scenarios"].as_array().expect("result scenarios");
        let expected_revisions = [21, 31, 19, 19, 31];
        let expected_terminal_messages = [1, 2, 1, 1, 2];
        for (((scenario, id), revision), terminal_messages) in scenarios
            .iter()
            .zip(ids)
            .zip(expected_revisions)
            .zip(expected_terminal_messages)
        {
            assert_eq!(scenario["id"], format!("renamed-{id}"));
            assert_eq!(scenario["observation"]["snapshot"]["revision"], revision);
            let commands = scenario["commands"].as_array().expect("commands");
            assert_eq!(
                commands
                    .iter()
                    .filter(|command| command["kind"] == "execution-port.message"
                        && command["request"]["kind"] == "job.outcome")
                    .count(),
                terminal_messages
            );
            for (index, terminal) in commands.iter().enumerate().filter(|(_, command)| {
                command["kind"] == "execution-port.message"
                    && command["request"]["kind"] == "job.outcome"
            }) {
                assert_eq!(
                    terminal["response"]["commits"][0]["operation"],
                    "apply_terminal_outcome"
                );
                let source = &terminal["sourceCommandIndexes"];
                assert!(commands[index + 1..].iter().any(|command| {
                    command["kind"] == "control-plane.command"
                        && command["request"]["command"] == "delivery.submit_verdict"
                        && &command["sourceCommandIndexes"] == source
                }));
            }
        }
    }

    #[test]
    fn non_verdict_scenarios_execute_through_the_typed_control_plane() {
        let oracle: Value = serde_json::from_slice(include_bytes!(
            "../../../../tests/fixtures/oracles/delivery-strongflow-typescript.v1.json"
        ))
        .expect("oracle");
        let ids = [
            "request-id-replay",
            "revision-conflict",
            "corruption-recovery",
            "task-dag",
            "attention",
        ];
        let root = std::env::temp_dir().join(format!(
            "winwincode-non-verdict-focused-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .expect("clock")
                .as_nanos()
        ));
        let scenarios = oracle["scenarios"]
            .as_array()
            .expect("scenarios")
            .iter()
            .filter(|scenario| scenario["id"].as_str().is_some_and(|id| ids.contains(&id)))
            .map(|scenario| {
                let commands = scenario["commands"]
                    .as_array()
                    .expect("commands")
                    .iter()
                    .map(|command| {
                        let mut projected = serde_json::Map::new();
                        projected.insert("kind".to_owned(), command["kind"].clone());
                        if let Some(request) = command.get("request") {
                            projected.insert("request".to_owned(), request.clone());
                        }
                        if let Some(input) = command.get("input") {
                            projected.insert("input".to_owned(), input.clone());
                        }
                        Value::Object(projected)
                    })
                    .collect::<Vec<_>>();
                json!({
                    "id": scenario["id"],
                    "commands": commands,
                    "terminalOutcomeStatusBySourceCommandIndex":
                        local_fixture_terminal_outcome_statuses(scenario)
                            .expect("terminal outcome plan facts"),
                })
            })
            .collect::<Vec<_>>();
        let serialized = serde_json::to_string(&json!({
            "schemaVersion": PLAN_SCHEMA,
            "oracleSchemaVersion": oracle["schemaVersion"],
            "bindings": {
                "ORACLE_ROOT": root,
                "NODE_EXECUTABLE": "/usr/bin/node",
                "AUTH_PROOF": "non-verdict-focused-proof",
                "fixtureRandomIdentities": {}
            },
            "scenarios": scenarios
        }))
        .expect("plan JSON")
        .replace("<ORACLE_ROOT>", &root.to_string_lossy())
        .replace("<NODE_EXECUTABLE>", "/usr/bin/node")
        .replace("<AUTH_PROOF>", "non-verdict-focused-proof");
        let plan: DifferentialPlan = serde_json::from_str(&serialized).expect("plan");

        let result = run_differential_plan(&plan).expect("non-verdict execution");
        let scenarios = result["scenarios"].as_array().expect("result scenarios");
        let expected_revisions = [1, 2, 1, 2, 8];
        for ((scenario, id), revision) in scenarios.iter().zip(ids).zip(expected_revisions) {
            assert_eq!(scenario["id"], id);
            assert_eq!(scenario["observation"]["snapshot"]["revision"], revision);
        }
        for scenario in &scenarios[..4] {
            assert_eq!(
                scenario["observation"]["projection"]["runtime"],
                Value::Null
            );
        }
        assert_ne!(
            scenarios[4]["observation"]["projection"]["runtime"],
            Value::Null
        );
    }

    #[test]
    fn task_dag_cycle_maps_to_sealed_zero_write_promotion_rejection() {
        let oracle: Value = serde_json::from_slice(include_bytes!(
            "../../../../tests/fixtures/oracles/delivery-strongflow-typescript.v1.json"
        ))
        .expect("oracle");
        let source_scenario = oracle["scenarios"]
            .as_array()
            .expect("scenarios")
            .iter()
            .find(|scenario| scenario["id"] == "task-dag")
            .expect("task-dag")
            .clone();
        let commands = source_scenario["commands"]
            .as_array()
            .expect("commands")
            .iter()
            .map(|command| {
                let mut projected = serde_json::Map::new();
                projected.insert("kind".to_owned(), command["kind"].clone());
                if let Some(request) = command.get("request") {
                    projected.insert("request".to_owned(), request.clone());
                }
                if let Some(input) = command.get("input") {
                    projected.insert("input".to_owned(), input.clone());
                }
                Value::Object(projected)
            })
            .collect::<Vec<_>>();
        let scenario = json!({
            "id": "renamed-task-graph",
            "commands": commands,
            "terminalOutcomeStatusBySourceCommandIndex": {},
        });
        let root = std::env::temp_dir().join(format!(
            "winwincode-task-dag-focused-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .expect("clock")
                .as_nanos()
        ));
        let plan: DifferentialPlan = serde_json::from_value(json!({
            "schemaVersion": PLAN_SCHEMA,
            "oracleSchemaVersion": oracle["schemaVersion"],
            "bindings": {
                "ORACLE_ROOT": root,
                "NODE_EXECUTABLE": "/usr/bin/node",
                "AUTH_PROOF": "task-dag-focused-proof",
                "fixtureRandomIdentities": {}
            },
            "scenarios": [scenario]
        }))
        .expect("plan");

        let result = run_differential_plan(&plan).expect("task-dag execution");
        let scenario = &result["scenarios"][0];
        assert_eq!(scenario["id"], "renamed-task-graph");
        let cycle = scenario["commands"]
            .as_array()
            .expect("commands")
            .iter()
            .find(|command| command["sourceCommandIndexes"] == json!([2]))
            .expect("cycle migration");
        assert_eq!(cycle["kind"], "fixture.command");
        assert_eq!(cycle["request"]["kind"], "fixture.solution-review.validate");
        assert_eq!(
            cycle["request"]["input"]["invalidProposalKind"],
            "dependency-cycle"
        );
        assert_eq!(cycle["response"]["error"]["code"], "INVALID_REQUEST");
        assert_eq!(
            cycle["response"]["error"]["message"],
            "solution-review task proposal dependencies contain a cycle"
        );
        assert_eq!(
            scenario["observation"]["snapshot"]["id"],
            "dlv_5QNEEJDDVR7MC02RM22SXW5TMJ"
        );
        assert_eq!(scenario["observation"]["snapshot"]["revision"], 2);
        assert_eq!(
            scenario["observation"]["store"]["journal"]["records"]
                .as_array()
                .expect("records")
                .len(),
            2
        );
    }
}
