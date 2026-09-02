// SPDX-License-Identifier: Apache-2.0

//! One-time conversion of the legacy TypeScript Delivery snapshot.
//!
//! This module owns the only old-session conversion entry point. It accepts a
//! complete legacy Delivery snapshot, keeps the Delivery graph's existing
//! `StageRun` and binding identities, and writes a canonical snapshot through a
//! caller-owned transaction. The transaction must atomically persist both the
//! converted snapshot and its consumed marker.

use std::collections::{HashMap, HashSet};
use std::fmt;

use serde::Deserialize;
use serde::de::{self, Deserializer, MapAccess, SeqAccess, Visitor};
use serde_json::{Map, Value};
use sha2::{Digest, Sha256};
use winwincode_domain::is_canonical_delivery_id;

/// Version shared with the Delivery cutover mapping authority.
pub const MIGRATION_SCHEMA_VERSION: &str = "winwincode.delivery-strongflow-legacy-to-canonical.v1";

const DELIVERY_SCHEMA_VERSION: u64 = 3;
const MAX_LEGACY_IDENTIFIER_LENGTH: usize = 200;

const ROOT_FIELDS: &[&str] = &[
    "attentionItems",
    "createdAtMillis",
    "evidence",
    "id",
    "revision",
    "schemaVersion",
    "sessionBindings",
    "spec",
    "stageRuns",
    "status",
    "tasks",
    "updatedAtMillis",
    "verdict",
];

const STAGE_RUN_FIELDS: &[&str] = &[
    "actorType",
    "attempt",
    "deliveryId",
    "deliveryTaskId",
    "finishedAtMillis",
    "id",
    "role",
    "schemaVersion",
    "stage",
    "startedAtMillis",
    "status",
];

const LEGACY_BINDING_FIELDS: &[&str] = &[
    "boundAtMillis",
    "codexSessionId",
    "deliveryId",
    "dshSessionId",
    "id",
    "schemaVersion",
    "stageRunId",
];

const CANONICAL_BINDING_FIELDS: &[&str] = &[
    "productSessionId",
    "executionJobId",
    "workerSessionId",
    "codexThreadId",
    "workerId",
    "workerInstanceId",
    "leaseId",
    "attempt",
    "fencingToken",
    "sourceProvenance",
];

/// Result of the atomic marker-and-snapshot transaction.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MigrationCommit {
    /// The canonical snapshot and its consumed marker were committed together.
    Applied,
    /// The consumed marker already existed and the durable snapshot was read.
    AlreadyConsumed {
        /// Exact canonical bytes written by the first successful transaction.
        canonical_snapshot: Vec<u8>,
    },
}

/// Explicit result of consuming one legacy source.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MigrationOutcome {
    /// This call atomically stored the canonical snapshot and consumed marker.
    Applied {
        /// Durable key identifying the one-time legacy source.
        source_key: String,
        /// Exact canonical snapshot bytes stored by this call.
        canonical_snapshot: Vec<u8>,
    },
    /// A prior call consumed the source; no write was performed.
    AlreadyConsumed {
        /// Durable key identifying the one-time legacy source.
        source_key: String,
        /// Exact canonical bytes read from the first successful transaction.
        canonical_snapshot: Vec<u8>,
    },
}

/// Closed failure returned by the durable migration transaction.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MigrationTransactionError {
    /// Durable rows violate the all-or-nothing migration invariant.
    CorruptState { message: String },
    /// The storage engine could not complete the transaction.
    Storage { message: String },
}

impl fmt::Display for MigrationTransactionError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::CorruptState { message } => {
                write!(formatter, "corrupt migration state: {message}")
            }
            Self::Storage { message } => write!(formatter, "migration storage failed: {message}"),
        }
    }
}

impl std::error::Error for MigrationTransactionError {}

/// Storage boundary for one-time migration.
///
/// Implementations persist `canonical_snapshot`, the `source_key` marker, and
/// the consumed marker in one atomic transaction. They must return
/// `MigrationCommit::AlreadyConsumed` when that marker already exists, return
/// the original durable snapshot, and leave every record unchanged when
/// returning an error.
pub trait MigrationTransaction {
    /// Atomically commit one converted snapshot and its source marker.
    fn commit_once(
        &mut self,
        source_key: &str,
        canonical_snapshot: &[u8],
    ) -> Result<MigrationCommit, MigrationTransactionError>;
}

/// Failure returned when the old snapshot cannot be converted or committed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MigrationError {
    InvalidShape { message: String },
    MissingField { field: String },
    InvalidValue { field: String, message: String },
    MixedIdentityShape { field: String },
    UnknownField { field: String },
    AmbiguousInput { message: String },
    InvalidJson { message: String },
    Serialization { message: String },
    Transaction { message: String },
    CorruptState { message: String },
}

impl fmt::Display for MigrationError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidShape { message } => {
                write!(formatter, "invalid migration shape: {message}")
            }
            Self::MissingField { field } => write!(formatter, "legacy value lacks field {field}"),
            Self::InvalidValue { field, message } => {
                write!(formatter, "invalid legacy field {field}: {message}")
            }
            Self::MixedIdentityShape { field } => {
                write!(formatter, "legacy value contains canonical field {field}")
            }
            Self::UnknownField { field } => write!(formatter, "unknown legacy field {field}"),
            Self::AmbiguousInput { message } => {
                write!(formatter, "ambiguous legacy input: {message}")
            }
            Self::InvalidJson { message } => {
                write!(formatter, "invalid migration JSON: {message}")
            }
            Self::Serialization { message } => {
                write!(
                    formatter,
                    "canonical migration serialization failed: {message}"
                )
            }
            Self::Transaction { message } => {
                write!(formatter, "migration transaction failed: {message}")
            }
            Self::CorruptState { message } => {
                write!(formatter, "corrupt migration state: {message}")
            }
        }
    }
}

impl std::error::Error for MigrationError {}

/// Convert one complete legacy Delivery snapshot.
///
/// The function validates and transforms the complete graph before invoking
/// the transaction. Human `StageRuns` remain in the graph, while their old
/// `SessionBindings` are removed because human review is a `ProductSession` action
/// rather than an ExecutionJob-backed binding. Codex bindings retain their old
/// binding and `StageRun` identities; only `ProductSessionId` and `ExecutionJobId`
/// are generated. Worker/Codex/lease authority remains empty until its owning
/// runtime reports it.
pub fn migrate_legacy_delivery_json<T: MigrationTransaction>(
    input: &[u8],
    transaction: &mut T,
) -> Result<MigrationOutcome, MigrationError> {
    let mut snapshot = parse_json(input)?;
    let source_key = transform_snapshot(&mut snapshot)?;
    let canonical_snapshot =
        serde_json::to_vec(&snapshot).map_err(|error| MigrationError::Serialization {
            message: error.to_string(),
        })?;

    let commit = transaction
        .commit_once(&source_key, &canonical_snapshot)
        .map_err(|error| match error {
            MigrationTransactionError::CorruptState { message } => {
                MigrationError::CorruptState { message }
            }
            MigrationTransactionError::Storage { message } => {
                MigrationError::Transaction { message }
            }
        })?;
    match commit {
        MigrationCommit::Applied => Ok(MigrationOutcome::Applied {
            source_key,
            canonical_snapshot,
        }),
        MigrationCommit::AlreadyConsumed { canonical_snapshot } => {
            Ok(MigrationOutcome::AlreadyConsumed {
                source_key,
                canonical_snapshot,
            })
        }
    }
}

fn transform_snapshot(snapshot: &mut Value) -> Result<String, MigrationError> {
    let object = snapshot
        .as_object_mut()
        .ok_or_else(|| MigrationError::InvalidShape {
            message: "legacy Delivery must be a JSON object".to_owned(),
        })?;

    if object.contains_key("migrationVersion") {
        return Err(MigrationError::MixedIdentityShape {
            field: "migrationVersion".to_owned(),
        });
    }
    ensure_exact_fields(object, ROOT_FIELDS, "delivery")?;
    require_schema_version(object, "delivery")?;

    let delivery_id = required_text(object, "id")?;
    if !is_canonical_delivery_id(&delivery_id) {
        return Err(MigrationError::InvalidValue {
            field: "id".to_owned(),
            message: "must use the canonical Delivery identifier".to_owned(),
        });
    }

    let stage_runs = required_array(object, "stageRuns")?;
    let runs_by_id = parse_stage_runs(stage_runs, &delivery_id)?;
    let old_bindings = required_array(object, "sessionBindings")?;
    let migrated = migrate_bindings(old_bindings, &delivery_id, &runs_by_id)?;
    validate_cross_references(
        object,
        &runs_by_id,
        &migrated.retained_binding_ids,
        &migrated.dropped_human_binding_ids,
    )?;

    object.insert(
        "sessionBindings".to_owned(),
        Value::Array(migrated.bindings),
    );

    Ok(source_key(&delivery_id))
}

#[derive(Debug, Clone)]
struct StageRunInfo {
    delivery_task_id: Value,
    actor_type: ActorType,
    attempt: Value,
    started_at_millis: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ActorType {
    Codex,
    Human,
}

#[derive(Debug, Default)]
struct MigratedBindings {
    bindings: Vec<Value>,
    retained_binding_ids: HashSet<String>,
    dropped_human_binding_ids: HashSet<String>,
}

#[derive(Debug)]
struct LegacyBinding {
    binding_id: String,
    stage_run_id: String,
    dsh_session_id: Option<String>,
    actor_type: ActorType,
    bound_at_millis: u64,
}

fn parse_stage_runs(
    stage_runs: &[Value],
    delivery_id: &str,
) -> Result<HashMap<String, StageRunInfo>, MigrationError> {
    let mut by_id = HashMap::with_capacity(stage_runs.len());
    for (index, run) in stage_runs.iter().enumerate() {
        let path = format!("stageRuns[{index}]");
        let object = run
            .as_object()
            .ok_or_else(|| MigrationError::InvalidShape {
                message: format!("{path} must be an object"),
            })?;
        ensure_exact_fields(object, STAGE_RUN_FIELDS, &path)?;
        require_schema_version(object, &path)?;
        let id = required_text(object, "id")?;
        if by_id.contains_key(&id) {
            return Err(MigrationError::AmbiguousInput {
                message: format!("duplicate StageRun id {id}"),
            });
        }
        let run_delivery_id = required_text(object, "deliveryId")?;
        if run_delivery_id != delivery_id {
            return Err(MigrationError::InvalidValue {
                field: format!("{path}.deliveryId"),
                message: "must match the Delivery id".to_owned(),
            });
        }
        let delivery_task_id = required_nullable_text(object, "deliveryTaskId")?;
        let delivery_task_id = delivery_task_id.map_or(Value::Null, Value::String);
        let actor_type = match required_text(object, "actorType")?.as_str() {
            "codex" => ActorType::Codex,
            "human" => ActorType::Human,
            _ => {
                return Err(MigrationError::InvalidValue {
                    field: format!("{path}.actorType"),
                    message: "expected codex or human".to_owned(),
                });
            }
        };
        let stage = required_text(object, "stage")?;
        if !matches!(
            stage.as_str(),
            "clarifying"
                | "planning"
                | "plan-review"
                | "executing"
                | "verifying"
                | "reworking"
                | "delivery-review"
        ) {
            return Err(MigrationError::InvalidValue {
                field: format!("{path}.stage"),
                message: "unknown Delivery stage".to_owned(),
            });
        }
        required_text(object, "role")?;
        let status = required_text(object, "status")?;
        if !matches!(
            status.as_str(),
            "running" | "waiting" | "succeeded" | "failed" | "cancelled"
        ) {
            return Err(MigrationError::InvalidValue {
                field: format!("{path}.status"),
                message: "unknown StageRun status".to_owned(),
            });
        }
        required_positive_u64(object, "attempt")?;
        let attempt_value =
            object
                .get("attempt")
                .cloned()
                .ok_or_else(|| MigrationError::MissingField {
                    field: format!("{path}.attempt"),
                })?;
        let started_at_millis = required_u64(object, "startedAtMillis")?;
        let finished_at_millis = required_nullable_u64(object, "finishedAtMillis")?;
        let active = matches!(status.as_str(), "running" | "waiting");
        if active == finished_at_millis.is_some()
            || finished_at_millis.is_some_and(|finished| finished < started_at_millis)
        {
            return Err(MigrationError::InvalidValue {
                field: format!("{path}.finishedAtMillis"),
                message: "finish time does not match StageRun status".to_owned(),
            });
        }
        by_id.insert(
            id,
            StageRunInfo {
                delivery_task_id,
                actor_type,
                attempt: attempt_value,
                started_at_millis,
            },
        );
    }
    Ok(by_id)
}

fn migrate_bindings(
    bindings: &[Value],
    delivery_id: &str,
    runs_by_id: &HashMap<String, StageRunInfo>,
) -> Result<MigratedBindings, MigrationError> {
    let mut result = MigratedBindings {
        bindings: Vec::with_capacity(bindings.len()),
        retained_binding_ids: HashSet::with_capacity(bindings.len()),
        dropped_human_binding_ids: HashSet::new(),
    };
    let mut all_binding_ids = HashSet::with_capacity(bindings.len());

    for (index, binding) in bindings.iter().enumerate() {
        let path = format!("sessionBindings[{index}]");
        let legacy = parse_legacy_binding(binding, &path, delivery_id, runs_by_id)?;
        if !all_binding_ids.insert(legacy.binding_id.clone()) {
            return Err(MigrationError::AmbiguousInput {
                message: format!("duplicate SessionBinding id {}", legacy.binding_id),
            });
        }
        if legacy.actor_type == ActorType::Human {
            result.dropped_human_binding_ids.insert(legacy.binding_id);
            continue;
        }

        let canonical = canonical_binding(&legacy, delivery_id, runs_by_id)?;
        result
            .retained_binding_ids
            .insert(legacy.binding_id.clone());
        result.bindings.push(canonical);
    }

    Ok(result)
}

fn parse_legacy_binding(
    binding: &Value,
    path: &str,
    delivery_id: &str,
    runs_by_id: &HashMap<String, StageRunInfo>,
) -> Result<LegacyBinding, MigrationError> {
    let object = binding
        .as_object()
        .ok_or_else(|| MigrationError::InvalidShape {
            message: format!("{path} must be an object"),
        })?;
    for field in CANONICAL_BINDING_FIELDS {
        if object.contains_key(*field) {
            return Err(MigrationError::MixedIdentityShape {
                field: format!("{path}.{field}"),
            });
        }
    }
    ensure_exact_fields(object, LEGACY_BINDING_FIELDS, path)?;
    require_schema_version(object, path)?;
    let binding_id = required_text(object, "id")?;
    let binding_delivery_id = required_text(object, "deliveryId")?;
    if binding_delivery_id != delivery_id {
        return Err(MigrationError::InvalidValue {
            field: format!("{path}.deliveryId"),
            message: "must match the Delivery id".to_owned(),
        });
    }
    let stage_run_id = required_text(object, "stageRunId")?;
    let run = runs_by_id
        .get(&stage_run_id)
        .ok_or_else(|| MigrationError::InvalidValue {
            field: format!("{path}.stageRunId"),
            message: "must reference a StageRun in the same Delivery".to_owned(),
        })?;
    let dsh_session_id = required_nullable_text(object, "dshSessionId")?;
    let codex_session_id = required_nullable_text(object, "codexSessionId")?;
    if dsh_session_id.is_none() {
        return Err(MigrationError::InvalidValue {
            field: format!("{path}.dshSessionId"),
            message: "must identify the ProductSession".to_owned(),
        });
    }
    match run.actor_type {
        ActorType::Codex if codex_session_id.is_none() => {
            return Err(MigrationError::InvalidValue {
                field: format!("{path}.codexSessionId"),
                message: "Codex StageRun requires a Codex session".to_owned(),
            });
        }
        ActorType::Human if codex_session_id.is_some() => {
            return Err(MigrationError::InvalidValue {
                field: format!("{path}.codexSessionId"),
                message: "Human StageRun must not carry a Codex session".to_owned(),
            });
        }
        _ => {}
    }
    let bound_at_millis = required_u64(object, "boundAtMillis")?;
    if bound_at_millis < run.started_at_millis {
        return Err(MigrationError::InvalidValue {
            field: format!("{path}.boundAtMillis"),
            message: "must not precede the StageRun start".to_owned(),
        });
    }

    Ok(LegacyBinding {
        binding_id,
        stage_run_id,
        dsh_session_id,
        actor_type: run.actor_type,
        bound_at_millis,
    })
}

fn canonical_binding(
    binding: &LegacyBinding,
    delivery_id: &str,
    runs_by_id: &HashMap<String, StageRunInfo>,
) -> Result<Value, MigrationError> {
    let dsh_session_id = binding
        .dsh_session_id
        .as_deref()
        .expect("validated DSH session identity");
    let source_reference = format!("legacy:{binding_id}", binding_id = binding.binding_id);
    if !is_portable_identifier(&source_reference) {
        return Err(MigrationError::InvalidValue {
            field: format!("{}.id", binding.binding_id),
            message: "binding id is too long for canonical provenance".to_owned(),
        });
    }
    let run = runs_by_id
        .get(&binding.stage_run_id)
        .expect("validated StageRun identity");
    let mut canonical = Map::new();
    canonical.insert(
        "schemaVersion".to_owned(),
        Value::Number(serde_json::Number::from(DELIVERY_SCHEMA_VERSION)),
    );
    canonical.insert("id".to_owned(), Value::String(binding.binding_id.clone()));
    canonical.insert(
        "deliveryId".to_owned(),
        Value::String(delivery_id.to_owned()),
    );
    canonical.insert("deliveryTaskId".to_owned(), run.delivery_task_id.clone());
    canonical.insert(
        "stageRunId".to_owned(),
        Value::String(binding.stage_run_id.clone()),
    );
    canonical.insert(
        "productSessionId".to_owned(),
        Value::String(migration_id("psn_", dsh_session_id)),
    );
    canonical.insert(
        "executionJobId".to_owned(),
        Value::String(migration_job_id(
            delivery_id,
            &binding.stage_run_id,
            &binding.binding_id,
        )),
    );
    canonical.insert("workerSessionId".to_owned(), Value::Null);
    canonical.insert("codexThreadId".to_owned(), Value::Null);
    canonical.insert("workerId".to_owned(), Value::Null);
    canonical.insert("workerInstanceId".to_owned(), Value::Null);
    canonical.insert("leaseId".to_owned(), Value::Null);
    canonical.insert("attempt".to_owned(), run.attempt.clone());
    canonical.insert("fencingToken".to_owned(), Value::Null);
    let mut provenance = Map::new();
    provenance.insert(
        "kind".to_owned(),
        Value::String("legacy-migration".to_owned()),
    );
    provenance.insert("reference".to_owned(), Value::String(source_reference));
    canonical.insert("sourceProvenance".to_owned(), Value::Object(provenance));
    canonical.insert(
        "boundAtMillis".to_owned(),
        Value::Number(serde_json::Number::from(binding.bound_at_millis)),
    );
    Ok(Value::Object(canonical))
}

fn validate_cross_references(
    delivery: &Map<String, Value>,
    runs_by_id: &HashMap<String, StageRunInfo>,
    retained_binding_ids: &HashSet<String>,
    dropped_human_binding_ids: &HashSet<String>,
) -> Result<(), MigrationError> {
    let evidence = required_array(delivery, "evidence")?;
    for (index, reference) in evidence.iter().enumerate() {
        let path = format!("evidence[{index}]");
        let object = reference
            .as_object()
            .ok_or_else(|| MigrationError::InvalidShape {
                message: format!("{path} must be an object"),
            })?;
        let stage_run_id = required_text(object, "stageRunId")?;
        if !runs_by_id.contains_key(&stage_run_id) {
            return Err(MigrationError::InvalidValue {
                field: format!("{path}.stageRunId"),
                message: "must reference a StageRun in the same Delivery".to_owned(),
            });
        }
        let binding_id = required_text(object, "sessionBindingId")?;
        if dropped_human_binding_ids.contains(&binding_id) {
            return Err(MigrationError::InvalidValue {
                field: format!("{path}.sessionBindingId"),
                message: "Human SessionBinding was removed during migration".to_owned(),
            });
        }
        if !retained_binding_ids.contains(&binding_id) {
            return Err(MigrationError::InvalidValue {
                field: format!("{path}.sessionBindingId"),
                message: "must reference a retained SessionBinding".to_owned(),
            });
        }
    }

    let attention_items = required_array(delivery, "attentionItems")?;
    for (index, item) in attention_items.iter().enumerate() {
        let path = format!("attentionItems[{index}]");
        let object = item
            .as_object()
            .ok_or_else(|| MigrationError::InvalidShape {
                message: format!("{path} must be an object"),
            })?;
        if let Some(stage_run_id) = optional_text(object, "stageRunId")?
            && !runs_by_id.contains_key(&stage_run_id)
        {
            return Err(MigrationError::InvalidValue {
                field: format!("{path}.stageRunId"),
                message: "must reference a StageRun in the same Delivery".to_owned(),
            });
        }
    }
    Ok(())
}

fn source_key(delivery_id: &str) -> String {
    format!("{MIGRATION_SCHEMA_VERSION}:{delivery_id}")
}

fn migration_job_id(delivery_id: &str, stage_run_id: &str, binding_id: &str) -> String {
    migration_id(
        "job_",
        &format!("{delivery_id}:{stage_run_id}:{binding_id}"),
    )
}

fn migration_id(prefix: &str, input: &str) -> String {
    let digest = format!("{:X}", Sha256::digest(input.as_bytes()));
    format!("{}{}", prefix, &digest[..26])
}

fn ensure_exact_fields(
    object: &Map<String, Value>,
    allowed: &[&str],
    path: &str,
) -> Result<(), MigrationError> {
    for field in object.keys() {
        if !allowed.contains(&field.as_str()) {
            return Err(MigrationError::UnknownField {
                field: format!("{path}.{field}"),
            });
        }
    }
    Ok(())
}

fn require_schema_version(object: &Map<String, Value>, path: &str) -> Result<(), MigrationError> {
    let version = required_u64(object, "schemaVersion")?;
    if version != DELIVERY_SCHEMA_VERSION {
        return Err(MigrationError::InvalidValue {
            field: format!("{path}.schemaVersion"),
            message: format!("must be {DELIVERY_SCHEMA_VERSION}"),
        });
    }
    Ok(())
}

fn required_array<'value>(
    object: &'value Map<String, Value>,
    field: &str,
) -> Result<&'value [Value], MigrationError> {
    object
        .get(field)
        .ok_or_else(|| MigrationError::MissingField {
            field: field.to_owned(),
        })?
        .as_array()
        .map(Vec::as_slice)
        .ok_or_else(|| MigrationError::InvalidValue {
            field: field.to_owned(),
            message: "expected an array".to_owned(),
        })
}

fn required_text(object: &Map<String, Value>, field: &str) -> Result<String, MigrationError> {
    optional_text(object, field)?.ok_or_else(|| MigrationError::InvalidValue {
        field: field.to_owned(),
        message: "must not be null".to_owned(),
    })
}

fn required_nullable_text(
    object: &Map<String, Value>,
    field: &str,
) -> Result<Option<String>, MigrationError> {
    let value = object
        .get(field)
        .ok_or_else(|| MigrationError::MissingField {
            field: field.to_owned(),
        })?;
    if value.is_null() {
        return Ok(None);
    }
    let text = value.as_str().ok_or_else(|| MigrationError::InvalidValue {
        field: field.to_owned(),
        message: "expected a string or null".to_owned(),
    })?;
    if !is_portable_identifier(text) {
        return Err(MigrationError::InvalidValue {
            field: field.to_owned(),
            message: "must be a portable identifier".to_owned(),
        });
    }
    Ok(Some(text.to_owned()))
}

fn optional_text(
    object: &Map<String, Value>,
    field: &str,
) -> Result<Option<String>, MigrationError> {
    let value = object
        .get(field)
        .ok_or_else(|| MigrationError::MissingField {
            field: field.to_owned(),
        })?;
    if value.is_null() {
        return Ok(None);
    }
    let text = value.as_str().ok_or_else(|| MigrationError::InvalidValue {
        field: field.to_owned(),
        message: "expected a string or null".to_owned(),
    })?;
    if !is_portable_identifier(text) {
        return Err(MigrationError::InvalidValue {
            field: field.to_owned(),
            message: "must be a portable identifier".to_owned(),
        });
    }
    Ok(Some(text.to_owned()))
}

fn required_u64(object: &Map<String, Value>, field: &str) -> Result<u64, MigrationError> {
    object
        .get(field)
        .ok_or_else(|| MigrationError::MissingField {
            field: field.to_owned(),
        })?
        .as_u64()
        .ok_or_else(|| MigrationError::InvalidValue {
            field: field.to_owned(),
            message: "expected a non-negative integer".to_owned(),
        })
}

fn required_positive_u64(object: &Map<String, Value>, field: &str) -> Result<u64, MigrationError> {
    let value = required_u64(object, field)?;
    if value == 0 {
        return Err(MigrationError::InvalidValue {
            field: field.to_owned(),
            message: "must be positive".to_owned(),
        });
    }
    Ok(value)
}

fn required_nullable_u64(
    object: &Map<String, Value>,
    field: &str,
) -> Result<Option<u64>, MigrationError> {
    let value = object
        .get(field)
        .ok_or_else(|| MigrationError::MissingField {
            field: field.to_owned(),
        })?;
    if value.is_null() {
        return Ok(None);
    }
    value
        .as_u64()
        .map(Some)
        .ok_or_else(|| MigrationError::InvalidValue {
            field: field.to_owned(),
            message: "expected a non-negative integer or null".to_owned(),
        })
}

fn is_portable_identifier(value: &str) -> bool {
    let mut bytes = value.bytes();
    bytes
        .next()
        .is_some_and(|first| first.is_ascii_alphanumeric())
        && value.len() <= MAX_LEGACY_IDENTIFIER_LENGTH
        && bytes.all(|byte| {
            byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'_' | b':' | b'/' | b'@' | b'-')
        })
}

fn parse_json(input: &[u8]) -> Result<Value, MigrationError> {
    let mut deserializer = serde_json::Deserializer::from_slice(input);
    let StrictValue(value) = StrictValue::deserialize(&mut deserializer).map_err(|error| {
        let message = error.to_string();
        if message.contains("duplicate object field") {
            MigrationError::AmbiguousInput { message }
        } else {
            MigrationError::InvalidJson { message }
        }
    })?;
    deserializer
        .end()
        .map_err(|error| MigrationError::InvalidJson {
            message: error.to_string(),
        })?;
    Ok(value)
}

/// JSON value deserializer that rejects duplicate object keys at every level.
struct StrictValue(Value);

impl<'de> Deserialize<'de> for StrictValue {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        deserializer.deserialize_any(StrictValueVisitor)
    }
}

struct StrictValueVisitor;

impl<'de> Visitor<'de> for StrictValueVisitor {
    type Value = StrictValue;

    fn expecting(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("a JSON value with unique object keys")
    }

    fn visit_bool<E>(self, value: bool) -> Result<Self::Value, E>
    where
        E: de::Error,
    {
        Ok(StrictValue(Value::Bool(value)))
    }

    fn visit_i64<E>(self, value: i64) -> Result<Self::Value, E>
    where
        E: de::Error,
    {
        Ok(StrictValue(Value::Number(value.into())))
    }

    fn visit_u64<E>(self, value: u64) -> Result<Self::Value, E>
    where
        E: de::Error,
    {
        Ok(StrictValue(Value::Number(value.into())))
    }

    fn visit_f64<E>(self, value: f64) -> Result<Self::Value, E>
    where
        E: de::Error,
    {
        serde_json::Number::from_f64(value)
            .map(|number| StrictValue(Value::Number(number)))
            .ok_or_else(|| E::custom("non-finite JSON number"))
    }

    fn visit_str<E>(self, value: &str) -> Result<Self::Value, E>
    where
        E: de::Error,
    {
        Ok(StrictValue(Value::String(value.to_owned())))
    }

    fn visit_string<E>(self, value: String) -> Result<Self::Value, E>
    where
        E: de::Error,
    {
        Ok(StrictValue(Value::String(value)))
    }

    fn visit_none<E>(self) -> Result<Self::Value, E>
    where
        E: de::Error,
    {
        Ok(StrictValue(Value::Null))
    }

    fn visit_unit<E>(self) -> Result<Self::Value, E>
    where
        E: de::Error,
    {
        Ok(StrictValue(Value::Null))
    }

    fn visit_some<D>(self, deserializer: D) -> Result<Self::Value, D::Error>
    where
        D: Deserializer<'de>,
    {
        StrictValue::deserialize(deserializer)
    }

    fn visit_seq<A>(self, mut access: A) -> Result<Self::Value, A::Error>
    where
        A: SeqAccess<'de>,
    {
        let mut values = Vec::new();
        while let Some(value) = access.next_element::<StrictValue>()? {
            values.push(value.0);
        }
        Ok(StrictValue(Value::Array(values)))
    }

    fn visit_map<A>(self, mut access: A) -> Result<Self::Value, A::Error>
    where
        A: MapAccess<'de>,
    {
        let mut object = Map::new();
        while let Some(key) = access.next_key::<String>()? {
            if object.contains_key(&key) {
                return Err(de::Error::custom(format!("duplicate object field {key}")));
            }
            let value = access.next_value::<StrictValue>()?;
            object.insert(key, value.0);
        }
        Ok(StrictValue(Value::Object(object)))
    }
}
