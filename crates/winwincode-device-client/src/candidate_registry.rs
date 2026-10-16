// SPDX-License-Identifier: Apache-2.0

use std::collections::HashSet;
use std::fmt;
use std::path::Path;
use std::process::Command;

use rusqlite::{OptionalExtension, TransactionBehavior, params};
use winwincode_client_port::domain::LocalCandidateState;
use winwincode_client_port::messages::{
    ClientCandidateRetainedPayload, ClientToServerMessage, CommandContext, OccupancyCommandContext,
};

use crate::daemon::DeviceDaemon;
use crate::store::{DeviceStore, DeviceStoreError};

pub const CANDIDATE_REF_PREFIX: &str = "refs/winwincode/candidates/";

const STATE_RETAINED: &str = "retained";

const MAX_ID_BYTES: usize = 200;
const MAX_REF_BYTES: usize = 200;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum CandidateRegistryErrorKind {
    InvalidInput,
    Conflict,
    NoOccupancyMirror,
    Store,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CandidateRegistryError {
    kind: CandidateRegistryErrorKind,
    message: String,
}

impl CandidateRegistryError {
    fn invalid(message: impl Into<String>) -> Self {
        Self {
            kind: CandidateRegistryErrorKind::InvalidInput,
            message: message.into(),
        }
    }

    fn conflict(message: impl Into<String>) -> Self {
        Self {
            kind: CandidateRegistryErrorKind::Conflict,
            message: message.into(),
        }
    }

    fn store(message: impl Into<String>) -> Self {
        Self {
            kind: CandidateRegistryErrorKind::Store,
            message: message.into(),
        }
    }

    #[must_use]
    pub const fn kind(&self) -> CandidateRegistryErrorKind {
        self.kind
    }

    #[must_use]
    pub fn message(&self) -> &str {
        &self.message
    }
}

impl fmt::Display for CandidateRegistryError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "{:?}: {}", self.kind, self.message)
    }
}

impl std::error::Error for CandidateRegistryError {}

impl From<DeviceStoreError> for CandidateRegistryError {
    fn from(error: DeviceStoreError) -> Self {
        Self::store(format!("device client store failure: {error}"))
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CandidateRetention {
    pub base_commit: String,
    pub candidate_commit: String,
    pub repository_binding_id: String,
    pub worker_session_id: String,
    pub local_git_ref: String,
    pub retained_at: String,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CandidateLocalRefRecord {
    pub base_commit: String,
    pub candidate_id: String,
    pub worker_session_id: String,
    pub repository_binding_id: String,
    pub local_git_ref: String,
    pub local_state: LocalCandidateState,
    pub created_at: String,
    pub candidate_ref: String,
    pub candidate_commit: String,
    pub retained_at: String,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum CandidateRetentionOutcome {
    Recorded(CandidateLocalRefRecord),
    Duplicate(CandidateLocalRefRecord),
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CandidateRetainReport {
    pub outcome: CandidateRetentionOutcome,
    pub frame_sequence: u64,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum CandidateRefVerdict {
    Verified,
    Missing,
    Drifted,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CandidateReconciliation {
    pub record: CandidateLocalRefRecord,
    pub verdict: CandidateRefVerdict,
    pub observed_commit: Option<String>,
}

pub fn record_candidate_retention(
    store: &mut DeviceStore,
    retention: &CandidateRetention,
) -> Result<CandidateRetentionOutcome, CandidateRegistryError> {
    validate_retention(retention)?;
    let candidate_id = retention.candidate_commit.clone();
    let connection = store.connection_mut()?;
    let transaction = connection
        .transaction_with_behavior(TransactionBehavior::Immediate)
        .map_err(crate::store::sql_error)?;
    let existing = read_candidate_row(&transaction, &candidate_id)?;
    if let Some(existing) = existing {
        ensure_same_candidate_facts(&existing, retention)?;
        transaction.commit().map_err(crate::store::sql_error)?;
        return Ok(CandidateRetentionOutcome::Duplicate(existing));
    }
    transaction
        .execute(
            "INSERT INTO candidate_local_refs \
             (candidate_id, worker_session_id, repository_binding_id, local_git_ref, \
              local_state, created_at, candidate_ref, candidate_commit, retained_at, base_commit) \
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10)",
            params![
                candidate_id,
                retention.worker_session_id,
                retention.repository_binding_id,
                retention.local_git_ref,
                STATE_RETAINED,
                retention.retained_at,
                retention.local_git_ref,
                retention.candidate_commit,
                retention.retained_at,
                retention.base_commit,
            ],
        )
        .map_err(crate::store::sql_error)?;
    transaction.commit().map_err(crate::store::sql_error)?;
    let record = candidate_local_ref(store, &candidate_id)?.ok_or_else(|| {
        CandidateRegistryError::store("recorded candidate row disappeared before the read-back")
    })?;
    Ok(CandidateRetentionOutcome::Recorded(record))
}

pub fn candidate_local_ref(
    store: &DeviceStore,
    candidate_id: &str,
) -> Result<Option<CandidateLocalRefRecord>, CandidateRegistryError> {
    if candidate_id.is_empty() {
        return Err(CandidateRegistryError::invalid(
            "candidate id must not be empty",
        ));
    }
    let connection = store.connection()?;
    connection
        .query_row(
            "SELECT candidate_id, worker_session_id, repository_binding_id, local_git_ref, \
             local_state, created_at, candidate_ref, candidate_commit, retained_at, base_commit \
             FROM candidate_local_refs WHERE candidate_id = ?1",
            params![candidate_id],
            row_to_candidate_record,
        )
        .optional()
        .map_err(crate::store::sql_error)
        .map_err(CandidateRegistryError::from)
}

pub fn retained_candidates(
    store: &DeviceStore,
) -> Result<Vec<CandidateLocalRefRecord>, CandidateRegistryError> {
    let mut statement = store
        .connection()?
        .prepare(
            "SELECT candidate_id, worker_session_id, repository_binding_id, local_git_ref, \
             local_state, created_at, candidate_ref, candidate_commit, retained_at, base_commit \
             FROM candidate_local_refs WHERE local_state = 'retained' \
             ORDER BY candidate_id",
        )
        .map_err(crate::store::sql_error)?;
    let rows = statement
        .query_map([], row_to_candidate_record)
        .map_err(crate::store::sql_error)?;
    rows.collect::<Result<Vec<_>, _>>()
        .map_err(crate::store::sql_error)
        .map_err(CandidateRegistryError::from)
}

pub fn retain_candidate(
    daemon: &mut DeviceDaemon,
    retention: &CandidateRetention,
) -> Result<CandidateRetainReport, CandidateRegistryError> {
    let outcome = record_candidate_retention(daemon.store_mut(), retention)?;
    let record = match &outcome {
        CandidateRetentionOutcome::Recorded(record)
        | CandidateRetentionOutcome::Duplicate(record) => record.clone(),
    };
    let frame_sequence = enqueue_candidate_retained(daemon, &record)?;
    Ok(CandidateRetainReport {
        outcome,
        frame_sequence,
    })
}

pub(crate) fn retain_worker_candidates(
    daemon: &mut DeviceDaemon,
    worker: &crate::store::WorkerProcessRecord,
) -> Result<(), CandidateRegistryError> {
    if daemon
        .occupancy_mirror()
        .is_none_or(|mirror| mirror.occupancy_lease_id != worker.occupancy_lease_id)
    {
        return Ok(());
    }
    let Some(mapping) = daemon
        .store_mut()
        .path_mapping(&worker.repository_binding_id)?
    else {
        return Ok(());
    };
    let git = |args: &[&str]| -> Result<Vec<u8>, CandidateRegistryError> {
        let output = Command::new("git")
            .arg("-C")
            .arg(&mapping.canonical_path)
            .env("GIT_TERMINAL_PROMPT", "0")
            .args(args)
            .output()
            .map_err(|error| CandidateRegistryError::store(error.to_string()))?;
        if !output.status.success() {
            return Err(CandidateRegistryError::store(
                "Cannot inspect frozen candidate refs",
            ));
        }
        Ok(output.stdout)
    };
    let refs = git(&[
        "for-each-ref",
        "--format=%(objectname)",
        CANDIDATE_REF_PREFIX,
    ])?;
    for commit in String::from_utf8_lossy(&refs).lines() {
        let message = git(&["show", "-s", "--format=%B", commit])?;
        let Ok(binding) = serde_json::from_slice::<serde_json::Value>(&message) else {
            continue;
        };
        let provenance = &binding["originProvenance"];
        if provenance["workerId"].as_str() != Some(worker.worker_id.as_str())
            || provenance["workerInstanceId"].as_str() != Some(worker.worker_instance_id.as_str())
        {
            continue;
        }
        retain_candidate(
            daemon,
            &CandidateRetention {
                base_commit: original_candidate_base(&binding, &git)?,
                candidate_commit: commit.to_owned(),
                repository_binding_id: worker.repository_binding_id.clone(),
                worker_session_id: worker.worker_session_id.clone(),
                local_git_ref: format!("{CANDIDATE_REF_PREFIX}{commit}"),
                retained_at: worker.last_observed_at.clone(),
            },
        )?;
    }
    Ok(())
}

fn original_candidate_base(
    binding: &serde_json::Value,
    git: &impl Fn(&[&str]) -> Result<Vec<u8>, CandidateRegistryError>,
) -> Result<String, CandidateRegistryError> {
    if binding["schemaVersion"].as_u64() != Some(1) {
        return Err(CandidateRegistryError::invalid(
            "the frozen candidate has no recognized source binding",
        ));
    }
    let repository = binding["repositoryId"].as_str().ok_or_else(|| {
        CandidateRegistryError::invalid("the frozen candidate has no repository identity")
    })?;
    if binding["originProvenance"]["workerId"]
        .as_str()
        .is_none_or(str::is_empty)
        || binding["originProvenance"]["workerInstanceId"]
            .as_str()
            .is_none_or(str::is_empty)
    {
        return Err(CandidateRegistryError::invalid(
            "the frozen candidate has no origin provenance",
        ));
    }
    let mut current = binding.clone();
    let mut visited = HashSet::new();
    loop {
        let source = current["sourceCommitId"].as_str().ok_or_else(|| {
            CandidateRegistryError::invalid("the frozen candidate has no persisted source commit")
        })?;
        validate_commit_sha(source, "candidate source commit")?;
        if !visited.insert(source.to_owned()) {
            return Err(CandidateRegistryError::invalid(
                "the candidate source binding chain contains a cycle",
            ));
        }
        let message = git(&["show", "-s", "--format=%B", source])?;
        let previous = serde_json::from_slice::<serde_json::Value>(&message).ok();
        let source_ref = format!("{CANDIDATE_REF_PREFIX}{source}");
        let retained_ref = git(&["for-each-ref", "--format=%(objectname)", &source_ref])?;
        let candidate_metadata = previous.as_ref().is_some_and(|previous| {
            previous.get("originProvenance").is_some() || previous.get("sourceCommitId").is_some()
        });
        if retained_ref.is_empty() && !candidate_metadata {
            return Ok(source.into());
        }
        let previous = previous.ok_or_else(|| {
            CandidateRegistryError::invalid(
                "a retained candidate source has corrupt binding metadata",
            )
        })?;
        if previous["schemaVersion"].as_u64() != Some(1)
            || previous["repositoryId"].as_str() != Some(repository)
            || previous["originProvenance"]["workerId"]
                .as_str()
                .is_none_or(str::is_empty)
            || previous["originProvenance"]["workerInstanceId"]
                .as_str()
                .is_none_or(str::is_empty)
            || previous["sourceCommitId"].as_str().is_none()
        {
            return Err(CandidateRegistryError::invalid(
                "the candidate source binding metadata is invalid",
            ));
        }
        current = previous;
    }
}

#[cfg(test)]
mod application_base_tests {
    use super::*;
    use std::fs;

    #[test]
    fn real_git_two_round_rework_preserves_the_persisted_original_base() {
        let root = std::env::temp_dir().join(format!(
            "wwc-candidate-source-{}-{}",
            std::process::id(),
            time::OffsetDateTime::now_utc().unix_timestamp_nanos()
        ));
        fs::create_dir_all(&root).unwrap();
        let git = |args: &[&str]| -> Result<Vec<u8>, CandidateRegistryError> {
            let output = Command::new("git")
                .arg("-C")
                .arg(&root)
                .args([
                    "-c",
                    "user.name=Source fixture",
                    "-c",
                    "user.email=source@example.test",
                ])
                .args(args)
                .output()
                .unwrap();
            if !output.status.success() {
                return Err(CandidateRegistryError::store(String::from_utf8_lossy(
                    &output.stderr,
                )));
            }
            Ok(output.stdout)
        };
        git(&["init", "-q", "--initial-branch=main"]).unwrap();
        fs::write(root.join("source.txt"), "source\n").unwrap();
        git(&["add", "-A"]).unwrap();
        git(&["commit", "-q", "-m", "source fixture"]).unwrap();
        let base = String::from_utf8(git(&["rev-parse", "HEAD"]).unwrap())
            .unwrap()
            .trim()
            .to_owned();
        let mut source = base.clone();
        for round in 0..3 {
            let binding = serde_json::json!({"schemaVersion":1, "repositoryId":"repo_fixture",
                "sourceCommitId":source, "originProvenance":{"workerId":"worker_fixture", "workerInstanceId":format!("instance_{round}")}});
            fs::write(root.join("candidate.txt"), format!("round {round}\n")).unwrap();
            git(&["add", "-A"]).unwrap();
            git(&["commit", "-q", "-m", &binding.to_string()]).unwrap();
            source = String::from_utf8(git(&["rev-parse", "HEAD"]).unwrap())
                .unwrap()
                .trim()
                .to_owned();
            git(&[
                "update-ref",
                &format!("{CANDIDATE_REF_PREFIX}{source}"),
                &source,
            ])
            .unwrap();
            assert_eq!(original_candidate_base(&binding, &git).unwrap(), base);
        }
        git(&["commit", "--allow-empty", "-q", "-m", "{}"]).unwrap();
        let corrupted = String::from_utf8(git(&["rev-parse", "HEAD"]).unwrap())
            .unwrap()
            .trim()
            .to_owned();
        git(&[
            "update-ref",
            &format!("{CANDIDATE_REF_PREFIX}{corrupted}"),
            &corrupted,
        ])
        .unwrap();
        let binding = serde_json::json!({"schemaVersion":1,"repositoryId":"repo_fixture", "sourceCommitId":corrupted,
            "originProvenance":{"workerId":"worker_fixture","workerInstanceId":"instance_fixture"}});
        assert!(original_candidate_base(&binding, &git).is_err());
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn cyclic_candidate_source_metadata_is_refused() {
        let binding = serde_json::json!({"schemaVersion":1,"repositoryId":"repo_fixture", "sourceCommitId":"a".repeat(40),
            "originProvenance":{"workerId":"worker_fixture","workerInstanceId":"instance_fixture"}});
        let error = original_candidate_base(&binding, &|args| {
            Ok(if args[0] == "show" {
                serde_json::to_vec(&binding).unwrap()
            } else {
                b"retained\n".to_vec()
            })
        })
        .unwrap_err();
        assert!(error.message().contains("cycle"));
    }
}

fn deterministic_lcr_id(candidate_id: &str) -> String {
    let hex: String = candidate_id
        .chars()
        .filter(char::is_ascii_hexdigit)
        .take(25)
        .map(|c| c.to_ascii_uppercase())
        .collect();
    format!("lcr_{hex}G")
}

pub fn enqueue_candidate_retained(
    daemon: &mut DeviceDaemon,
    record: &CandidateLocalRefRecord,
) -> Result<u64, CandidateRegistryError> {
    validate_record(record)?;
    if record.local_state != LocalCandidateState::Retained {
        return Err(CandidateRegistryError::invalid(format!(
            "candidate {} is in state {:?}; only a retained candidate is reported \
             as client.candidate.retained",
            record.candidate_id, record.local_state
        )));
    }
    let mirror = daemon
        .occupancy_mirror()
        .ok_or_else(|| CandidateRegistryError {
            kind: CandidateRegistryErrorKind::NoOccupancyMirror,
            message: "the device holds no occupancy mirror; the candidate retention \
                      cannot be stamped with a lease"
                .to_owned(),
        })?;
    let receipt = winwincode_client_port::domain::LocalCandidateReceipt {
        local_candidate_receipt_id: deterministic_lcr_id(&record.candidate_id),
        candidate_ref: record.candidate_ref.clone(),
        repository_binding_id: record.repository_binding_id.clone(),
        candidate_commit: record.candidate_commit.clone(),
        base_commit: record.base_commit.clone(),
        local_ref_name: record.local_git_ref.clone(),
        state: record.local_state,
        created_at: record.retained_at.clone(),
        revision: 1,
    };
    daemon
        .enqueue(ClientToServerMessage::CandidateRetained(
            ClientCandidateRetainedPayload {
                occupancy: OccupancyCommandContext {
                    command: CommandContext {
                        expected_revision: mirror.mirror_revision,
                        idempotency_key: format!("candidate-retained-{}", record.candidate_ref),
                    },
                    occupancy_lease_id: mirror.occupancy_lease_id.clone(),
                    occupancy_fencing_token: mirror.fencing_token,
                },
                worker_session_id: record.worker_session_id.clone(),
                receipt,
            },
        ))
        .map_err(|error| {
            CandidateRegistryError::store(format!(
                "the candidate retention frame cannot enter the durable outbox: {error:?}"
            ))
        })
}

pub fn reconcile_retained_candidates(
    store: &DeviceStore,
) -> Result<Vec<CandidateReconciliation>, CandidateRegistryError> {
    let mut reconciliations = Vec::new();
    for record in retained_candidates(store)? {
        let observed = read_candidate_ref_through_git(store, &record)?;
        let (verdict, observed_commit) = match observed {
            Some(commit) if commit == record.candidate_commit => {
                (CandidateRefVerdict::Verified, Some(commit))
            }
            Some(commit) => (CandidateRefVerdict::Drifted, Some(commit)),
            None => (CandidateRefVerdict::Missing, None),
        };
        reconciliations.push(CandidateReconciliation {
            record,
            verdict,
            observed_commit,
        });
    }
    Ok(reconciliations)
}

pub fn progress_candidate_lifecycle(
    store: &mut DeviceStore,
    candidate_id: &str,
    target: LocalCandidateState,
) -> Result<CandidateLocalRefRecord, CandidateRegistryError> {
    if candidate_id.is_empty() {
        return Err(CandidateRegistryError::invalid(
            "candidate id must not be empty",
        ));
    }
    let current = candidate_local_ref(store, candidate_id)?.ok_or_else(|| {
        CandidateRegistryError::invalid(format!(
            "candidate {candidate_id} is not retained on this device"
        ))
    })?;
    if current.local_state == target {
        return Ok(current);
    }
    let legal = match current.local_state {
        LocalCandidateState::Retained => matches!(
            target,
            LocalCandidateState::BranchCreated
                | LocalCandidateState::Applied
                | LocalCandidateState::Discarded
                | LocalCandidateState::Failed
        ),
        LocalCandidateState::BranchCreated => matches!(
            target,
            LocalCandidateState::Applied
                | LocalCandidateState::Discarded
                | LocalCandidateState::Failed
        ),
        LocalCandidateState::Failed => matches!(
            target,
            LocalCandidateState::Applied
                | LocalCandidateState::Discarded
                | LocalCandidateState::BranchCreated
        ),
        LocalCandidateState::Applied | LocalCandidateState::Discarded => false,
    };
    if !legal {
        return Err(CandidateRegistryError::conflict(format!(
            "candidate transition {:?} -> {target:?} is not a contract 6 transition",
            current.local_state
        )));
    }
    let updated = store
        .connection_mut()?
        .execute(
            "UPDATE candidate_local_refs SET local_state = ?2 \
             WHERE candidate_id = ?1 AND local_state = ?3",
            params![
                candidate_id,
                lifecycle_wire_name(target),
                lifecycle_wire_name(current.local_state)
            ],
        )
        .map_err(crate::store::sql_error)?;
    if updated != 1 {
        return Err(CandidateRegistryError::store(
            "the candidate lifecycle compare-and-swap lost inside one write",
        ));
    }
    candidate_local_ref(store, candidate_id)?.ok_or_else(|| {
        CandidateRegistryError::store(
            "the progressed candidate row disappeared before the read-back",
        )
    })
}

fn lifecycle_wire_name(state: LocalCandidateState) -> &'static str {
    match state {
        LocalCandidateState::Retained => STATE_RETAINED,
        LocalCandidateState::BranchCreated => "branch_created",
        LocalCandidateState::Applied => "applied",
        LocalCandidateState::Discarded => "discarded",
        LocalCandidateState::Failed => "failed",
    }
}

fn read_candidate_ref_through_git(
    store: &DeviceStore,
    record: &CandidateLocalRefRecord,
) -> Result<Option<String>, CandidateRegistryError> {
    let Some(mapping) = store.path_mapping(&record.repository_binding_id)? else {
        return Ok(None);
    };
    if !Path::new(&mapping.canonical_path).exists() {
        return Ok(None);
    }
    read_git_ref(&mapping.canonical_path, &record.local_git_ref)
}

fn read_git_ref(
    repository_path: &str,
    ref_name: &str,
) -> Result<Option<String>, CandidateRegistryError> {
    let mut command = Command::new("git");
    command.arg("-C").arg(repository_path);
    command.env("GIT_CONFIG_NOSYSTEM", "1");
    command.env("GIT_TERMINAL_PROMPT", "0");
    command.args([
        "rev-parse",
        "--verify",
        "--quiet",
        "--end-of-options",
        ref_name,
    ]);
    let output = command
        .output()
        .map_err(|error| CandidateRegistryError::store(format!("Git cannot be run: {error}")))?;
    if output.status.success() {
        let text = std::str::from_utf8(&output.stdout)
            .map_err(|_| CandidateRegistryError::invalid("Git ref output is not UTF-8"))?;
        let text = text.trim_end_matches(['\r', '\n']);
        if text.is_empty() || text.contains(['\r', '\n']) {
            return Err(CandidateRegistryError::store(
                "Git ref output is not a single identity",
            ));
        }
        return Ok(Some(text.to_owned()));
    }
    if output.status.code() == Some(1) {
        return Ok(None);
    }
    let stderr = String::from_utf8_lossy(&output.stderr);
    Err(CandidateRegistryError::store(format!(
        "Git candidate ref cannot be read: {}",
        stderr.trim()
    )))
}

fn validate_retention(retention: &CandidateRetention) -> Result<(), CandidateRegistryError> {
    validate_commit_sha(&retention.candidate_commit, "candidate commit")?;
    validate_commit_sha(&retention.base_commit, "candidate base commit")?;
    require_non_empty(
        &retention.repository_binding_id,
        "repository binding id",
        MAX_ID_BYTES,
    )?;
    require_non_empty(
        &retention.worker_session_id,
        "worker session id",
        MAX_ID_BYTES,
    )?;
    require_non_empty(&retention.retained_at, "retained at", MAX_ID_BYTES)?;
    validate_candidate_git_ref(&retention.local_git_ref, &retention.candidate_commit)
}

fn validate_record(record: &CandidateLocalRefRecord) -> Result<(), CandidateRegistryError> {
    require_non_empty(&record.candidate_id, "candidate id", MAX_ID_BYTES)?;
    require_non_empty(&record.worker_session_id, "worker session id", MAX_ID_BYTES)?;
    require_non_empty(
        &record.repository_binding_id,
        "repository binding id",
        MAX_ID_BYTES,
    )?;
    require_non_empty(&record.local_git_ref, "local git ref", MAX_ID_BYTES)?;
    require_non_empty(&record.created_at, "created at", MAX_ID_BYTES)?;
    require_non_empty(&record.candidate_ref, "candidate ref", MAX_ID_BYTES)?;
    require_non_empty(&record.retained_at, "retained at", MAX_ID_BYTES)?;
    validate_commit_sha(&record.candidate_commit, "candidate commit")?;
    validate_commit_sha(&record.base_commit, "candidate base commit")?;
    if record.candidate_id != record.candidate_commit {
        return Err(CandidateRegistryError::invalid(
            "candidate id must be exactly the frozen candidate commit",
        ));
    }
    validate_candidate_git_ref(&record.local_git_ref, &record.candidate_commit)?;
    if record.candidate_ref != record.local_git_ref {
        return Err(CandidateRegistryError::invalid(
            "candidate ref must be the canonical local candidate git ref",
        ));
    }
    Ok(())
}

fn ensure_same_candidate_facts(
    existing: &CandidateLocalRefRecord,
    retention: &CandidateRetention,
) -> Result<(), CandidateRegistryError> {
    let matches = existing.worker_session_id == retention.worker_session_id
        && existing.repository_binding_id == retention.repository_binding_id
        && existing.local_git_ref == retention.local_git_ref
        && existing.candidate_ref == retention.local_git_ref
        && existing.candidate_commit == retention.candidate_commit
        && existing.base_commit == retention.base_commit;
    if matches {
        return Ok(());
    }
    Err(CandidateRegistryError::conflict(format!(
        "candidate {} is already retained with different facts",
        existing.candidate_id
    )))
}

fn read_candidate_row(
    connection: &rusqlite::Connection,
    candidate_id: &str,
) -> Result<Option<CandidateLocalRefRecord>, DeviceStoreError> {
    connection
        .query_row(
            "SELECT candidate_id, worker_session_id, repository_binding_id, local_git_ref, \
             local_state, created_at, candidate_ref, candidate_commit, retained_at, base_commit \
             FROM candidate_local_refs WHERE candidate_id = ?1",
            params![candidate_id],
            row_to_candidate_record,
        )
        .optional()
        .map_err(crate::store::sql_error)
}

fn row_to_candidate_record(row: &rusqlite::Row<'_>) -> rusqlite::Result<CandidateLocalRefRecord> {
    let local_state: String = row.get(4)?;
    Ok(CandidateLocalRefRecord {
        candidate_id: row.get(0)?,
        worker_session_id: row.get(1)?,
        repository_binding_id: row.get(2)?,
        local_git_ref: row.get(3)?,
        local_state: parse_local_state(&local_state)?,
        created_at: row.get(5)?,
        candidate_ref: row.get(6)?,
        candidate_commit: row.get(7)?,
        retained_at: row.get(8)?,
        base_commit: row.get(9)?,
    })
}

fn parse_local_state(value: &str) -> rusqlite::Result<LocalCandidateState> {
    match value {
        "retained" => Ok(LocalCandidateState::Retained),
        "branch_created" => Ok(LocalCandidateState::BranchCreated),
        "applied" => Ok(LocalCandidateState::Applied),
        "discarded" => Ok(LocalCandidateState::Discarded),
        "failed" => Ok(LocalCandidateState::Failed),
        other => Err(rusqlite::Error::FromSqlConversionFailure(
            4,
            rusqlite::types::Type::Text,
            format!("stored candidate state {other} is not a plan 15 lifecycle state").into(),
        )),
    }
}

fn validate_commit_sha(value: &str, label: &str) -> Result<(), CandidateRegistryError> {
    let valid = (value.len() == 40 || value.len() == 64)
        && value
            .bytes()
            .all(|byte| matches!(byte, b'0'..=b'9' | b'a'..=b'f'));
    if valid {
        Ok(())
    } else {
        Err(CandidateRegistryError::invalid(format!(
            "{label} is not a full lowercase git commit id"
        )))
    }
}

fn validate_candidate_git_ref(
    local_git_ref: &str,
    candidate_commit: &str,
) -> Result<(), CandidateRegistryError> {
    let Some(suffix) = local_git_ref.strip_prefix(CANDIDATE_REF_PREFIX) else {
        return Err(CandidateRegistryError::invalid(format!(
            "local git ref is not inside the {CANDIDATE_REF_PREFIX} namespace"
        )));
    };
    let shaped = !suffix.is_empty()
        && suffix.len() <= MAX_REF_BYTES
        && suffix
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'_' | b'-'))
        && suffix.as_bytes()[0].is_ascii_alphanumeric();
    if !shaped {
        return Err(CandidateRegistryError::invalid(
            "local git ref suffix is not a canonical candidate name",
        ));
    }
    if suffix != candidate_commit {
        return Err(CandidateRegistryError::invalid(
            "local git ref does not name the frozen candidate commit",
        ));
    }
    Ok(())
}

fn require_non_empty(
    value: &str,
    label: &str,
    max_bytes: usize,
) -> Result<(), CandidateRegistryError> {
    if value.is_empty() {
        return Err(CandidateRegistryError::invalid(format!(
            "{label} must not be empty"
        )));
    }
    if value.len() > max_bytes {
        return Err(CandidateRegistryError::invalid(format!(
            "{label} must contain at most {max_bytes} bytes"
        )));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    const COMMIT: &str = "0f9e8d7c6b5a4938271605f4e3d2c1b0a9988776";

    #[test]
    fn a_canonical_candidate_ref_names_its_commit() {
        let reference = format!("{CANDIDATE_REF_PREFIX}{COMMIT}");
        assert_eq!(
            validate_candidate_git_ref(&reference, COMMIT),
            Ok(()),
            "the freeze-convention ref validates"
        );
    }

    #[test]
    fn a_foreign_or_mismatched_ref_is_rejected() {
        assert_eq!(
            validate_candidate_git_ref("refs/heads/main", COMMIT)
                .unwrap_err()
                .kind(),
            CandidateRegistryErrorKind::InvalidInput,
            "a ref outside the namespace fails closed"
        );
        let drifted = format!("{CANDIDATE_REF_PREFIX}{}", "not-the-commit");
        assert_eq!(
            validate_candidate_git_ref(&drifted, COMMIT)
                .unwrap_err()
                .kind(),
            CandidateRegistryErrorKind::InvalidInput,
            "a ref naming another candidate fails closed"
        );
    }

    #[test]
    fn commit_shapes_beyond_the_frozen_vocabulary_are_rejected() {
        assert_eq!(
            validate_commit_sha("0F9E8D7C6B5A4938271605F4E3D2C1B0A9988776", "commit")
                .unwrap_err()
                .kind(),
            CandidateRegistryErrorKind::InvalidInput,
            "uppercase hex is not the frozen shape"
        );
        assert_eq!(
            validate_commit_sha("0f9e8d7c", "commit")
                .unwrap_err()
                .kind(),
            CandidateRegistryErrorKind::InvalidInput,
            "an abbreviated commit is not a candidate identity"
        );
    }
}
