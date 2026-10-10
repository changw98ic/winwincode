use std::fmt;
use std::path::Path;
use std::process::Command;

use rusqlite::{OptionalExtension, params};
use winwincode_client_port::domain::{
    ApplyResult, ApplyStrategy, LocalApplyReceipt, LocalCandidateState,
};
use winwincode_client_port::messages::{
    ClientCandidateApplyResultPayload, ClientToServerMessage, CommandContext,
    OccupancyCommandContext,
};

use crate::candidate_registry::{
    CANDIDATE_REF_PREFIX, CandidateLocalRefRecord, candidate_local_ref,
    progress_candidate_lifecycle,
};
use crate::daemon::DeviceDaemon;
use crate::store::{DeviceStore, DeviceStoreError};

pub const WINWINCODE_BRANCH_PREFIX: &str = "winwincode/";

const MAX_SLUG_BYTES: usize = 48;

const MAX_BRANCH_SUFFIX_BYTES: usize = 160;

const MAX_ID_BYTES: usize = 200;

const SHORT_ID_RUNGS: [usize; 4] = [7, 12, 20, usize::MAX];

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CreatedBranchRecord {
    pub candidate_id: String,
    pub repository_binding_id: String,
    pub branch_name: String,
    pub created_at: String,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct BranchCreationRequest {
    pub repository_binding_id: String,
    pub candidate_ref: String,
    pub task_slug: String,
    pub requested_branch_name: Option<String>,
    pub requested_at: String,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum CandidateBranchErrorKind {
    InvalidInput,
    Conflict,
    NoOccupancyMirror,
    CandidateMissing,
    Store,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CandidateBranchError {
    kind: CandidateBranchErrorKind,
    message: String,
}

impl CandidateBranchError {
    fn invalid(message: impl Into<String>) -> Self {
        Self {
            kind: CandidateBranchErrorKind::InvalidInput,
            message: message.into(),
        }
    }

    fn conflict(message: impl Into<String>) -> Self {
        Self {
            kind: CandidateBranchErrorKind::Conflict,
            message: message.into(),
        }
    }

    fn candidate_missing(message: impl Into<String>) -> Self {
        Self {
            kind: CandidateBranchErrorKind::CandidateMissing,
            message: message.into(),
        }
    }

    fn store(message: impl Into<String>) -> Self {
        Self {
            kind: CandidateBranchErrorKind::Store,
            message: message.into(),
        }
    }

    #[must_use]
    pub const fn kind(&self) -> CandidateBranchErrorKind {
        self.kind
    }

    #[must_use]
    pub fn message(&self) -> &str {
        &self.message
    }
}

impl fmt::Display for CandidateBranchError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "{:?}: {}", self.kind, self.message)
    }
}

impl std::error::Error for CandidateBranchError {}

impl From<DeviceStoreError> for CandidateBranchError {
    fn from(error: DeviceStoreError) -> Self {
        Self::store(format!("device client store failure: {error}"))
    }
}

impl From<crate::candidate_registry::CandidateRegistryError> for CandidateBranchError {
    fn from(error: crate::candidate_registry::CandidateRegistryError) -> Self {
        Self {
            kind: match error.kind() {
                crate::candidate_registry::CandidateRegistryErrorKind::InvalidInput => {
                    CandidateBranchErrorKind::InvalidInput
                }
                crate::candidate_registry::CandidateRegistryErrorKind::Conflict => {
                    CandidateBranchErrorKind::Conflict
                }
                crate::candidate_registry::CandidateRegistryErrorKind::NoOccupancyMirror => {
                    CandidateBranchErrorKind::NoOccupancyMirror
                }
                crate::candidate_registry::CandidateRegistryErrorKind::Store => {
                    CandidateBranchErrorKind::Store
                }
            },
            message: error.message().to_owned(),
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct BranchCreationFacts {
    pub candidate_id: String,
    pub candidate_commit: String,
    pub candidate_ref: String,
    pub repository_binding_id: String,
    pub branch_name: String,
    pub branch_ref: String,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum BranchCreationOutcome {
    Created(BranchCreationFacts),
    Duplicate(BranchCreationFacts),
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct BranchCreationReport {
    pub outcome: BranchCreationOutcome,
    pub frame_sequence: u64,
}

pub fn create_candidate_branch(
    daemon: &mut DeviceDaemon,
    request: &BranchCreationRequest,
) -> Result<BranchCreationReport, CandidateBranchError> {
    validate_request(request)?;
    let candidate_id = candidate_id_of(&request.candidate_ref);
    let record = creation_guards(daemon, request, &candidate_id)?;
    let repository_path =
        bound_repository_path(daemon.store_mut(), &request.repository_binding_id)?;

    if let Some(durable) = created_branch_record(daemon.store_mut(), &candidate_id)? {
        let head = read_git_identity(&repository_path, &branch_ref_name(&durable.branch_name))?
            .ok_or_else(|| {
                CandidateBranchError::conflict(format!(
                    "the recorded branch {} no longer resolves; creation refuses \
                         to silently re-create it",
                    durable.branch_name
                ))
            })?;
        if head != record.candidate_commit {
            return Err(CandidateBranchError::conflict(format!(
                "the recorded branch {} drifted to another commit",
                durable.branch_name
            )));
        }
        let outcome = BranchCreationOutcome::Duplicate(facts_of(
            &record,
            &candidate_id,
            &durable.branch_name,
        ));
        return finish_creation(daemon, &record, &durable, outcome);
    }

    let observed =
        read_git_identity(&repository_path, &request.candidate_ref)?.ok_or_else(|| {
            CandidateBranchError::candidate_missing(format!(
                "the candidate ref {CANDIDATE_REF_PREFIX}{candidate_id} no longer resolves locally"
            ))
        })?;
    if observed != record.candidate_commit {
        return Err(CandidateBranchError::conflict(format!(
            "the candidate ref resolves to {observed}, not the retained commit {}",
            record.candidate_commit
        )));
    }

    let (branch_name, preexisting) = determine_branch_name(&repository_path, request, &record)?;
    if !preexisting {
        create_git_branch(&repository_path, &branch_name, &record.candidate_commit)?;
    }
    let outcome = if preexisting {
        BranchCreationOutcome::Duplicate(facts_of(&record, &candidate_id, &branch_name))
    } else {
        BranchCreationOutcome::Created(facts_of(&record, &candidate_id, &branch_name))
    };

    let durable = write_branch_record(
        daemon.store_mut(),
        &candidate_id,
        &request.repository_binding_id,
        &branch_name,
        &request.requested_at,
    )?;
    finish_creation(daemon, &record, &durable, outcome)
}

fn creation_guards(
    daemon: &mut DeviceDaemon,
    request: &BranchCreationRequest,
    candidate_id: &str,
) -> Result<CandidateLocalRefRecord, CandidateBranchError> {
    let record = candidate_local_ref(daemon.store_mut(), candidate_id)?.ok_or_else(|| {
        CandidateBranchError::candidate_missing(format!(
            "candidate ref {candidate_id} is not retained on this device"
        ))
    })?;
    if record.repository_binding_id != request.repository_binding_id {
        return Err(CandidateBranchError::conflict(format!(
            "candidate {candidate_id} is retained for binding {} and cannot branch on {}",
            record.repository_binding_id, request.repository_binding_id
        )));
    }
    if matches!(
        record.local_state,
        LocalCandidateState::Applied | LocalCandidateState::Discarded
    ) {
        return Err(CandidateBranchError::conflict(format!(
            "candidate {candidate_id} already reached the terminal state {:?}",
            record.local_state
        )));
    }
    if daemon.occupancy_mirror().is_none() {
        return Err(CandidateBranchError {
            kind: CandidateBranchErrorKind::NoOccupancyMirror,
            message: "the device holds no occupancy mirror; the branch creation \
                      cannot be stamped with a lease"
                .to_owned(),
        });
    }
    Ok(record)
}

fn determine_branch_name(
    repository_path: &str,
    request: &BranchCreationRequest,
    record: &CandidateLocalRefRecord,
) -> Result<(String, bool), CandidateBranchError> {
    match &request.requested_branch_name {
        Some(name) => {
            let head = read_git_identity(repository_path, &branch_ref_name(name))?;
            match head {
                Some(commit) if commit == record.candidate_commit => Ok((name.clone(), true)),
                Some(_) => Err(CandidateBranchError::conflict(format!(
                    "the requested branch {name} already exists pointing at another commit"
                ))),
                None => Ok((name.clone(), false)),
            }
        }
        None => derive_branch_name(
            repository_path,
            &request.task_slug,
            &record.candidate_commit,
        ),
    }
}

fn facts_of(
    record: &CandidateLocalRefRecord,
    candidate_id: &str,
    branch_name: &str,
) -> BranchCreationFacts {
    BranchCreationFacts {
        candidate_id: candidate_id.to_owned(),
        candidate_commit: record.candidate_commit.clone(),
        candidate_ref: record.candidate_ref.clone(),
        repository_binding_id: record.repository_binding_id.clone(),
        branch_name: branch_name.to_owned(),
        branch_ref: branch_ref_name(branch_name),
    }
}

fn finish_creation(
    daemon: &mut DeviceDaemon,
    record: &CandidateLocalRefRecord,
    durable: &CreatedBranchRecord,
    outcome: BranchCreationOutcome,
) -> Result<BranchCreationReport, CandidateBranchError> {
    let record = progress_candidate_lifecycle(
        daemon.store_mut(),
        &record.candidate_id,
        LocalCandidateState::BranchCreated,
    )?;
    let frame_sequence = enqueue_branch_created(daemon, &record, durable)?;
    Ok(BranchCreationReport {
        outcome,
        frame_sequence,
    })
}

fn deterministic_lar_id(candidate_id: &str, tag: char) -> String {
    let hex: String = candidate_id
        .chars()
        .filter(char::is_ascii_hexdigit)
        .take(25)
        .map(|c| c.to_ascii_uppercase())
        .collect();
    format!("lar_{hex}{tag}")
}

pub fn enqueue_branch_created(
    daemon: &mut DeviceDaemon,
    record: &CandidateLocalRefRecord,
    durable: &CreatedBranchRecord,
) -> Result<u64, CandidateBranchError> {
    let mirror = daemon
        .occupancy_mirror()
        .ok_or_else(|| CandidateBranchError {
            kind: CandidateBranchErrorKind::NoOccupancyMirror,
            message: "the device holds no occupancy mirror; the branch creation \
                  cannot be stamped with a lease"
                .to_owned(),
        })?;
    let receipt = LocalApplyReceipt {
        local_apply_receipt_id: deterministic_lar_id(&record.candidate_id, 'G'),
        candidate_ref: record.candidate_ref.clone(),
        repository_binding_id: record.repository_binding_id.clone(),
        target_branch: durable.branch_name.clone(),
        expected_head: record.candidate_commit.clone(),
        strategy: ApplyStrategy::CreateBranch,
        result: ApplyResult::BranchCreated,
        resulting_commit: Some(record.candidate_commit.clone()),
        conflict_artifact_ref: None,
        created_at: durable.created_at.clone(),
        revision: 1,
    };
    daemon
        .enqueue(ClientToServerMessage::CandidateApplyResult(
            ClientCandidateApplyResultPayload {
                occupancy: OccupancyCommandContext {
                    command: CommandContext {
                        expected_revision: mirror.mirror_revision,
                        idempotency_key: format!(
                            "candidate-branch-created-{}",
                            record.candidate_ref
                        ),
                    },
                    occupancy_lease_id: mirror.occupancy_lease_id.clone(),
                    occupancy_fencing_token: mirror.fencing_token,
                },
                receipt,
            },
        ))
        .map_err(|error| {
            CandidateBranchError::store(format!(
                "the branch creation frame cannot enter the durable outbox: {error:?}"
            ))
        })
}

pub fn created_branch_record(
    store: &mut DeviceStore,
    candidate_id: &str,
) -> Result<Option<CreatedBranchRecord>, CandidateBranchError> {
    ensure_branch_schema(store)?;
    let connection = store.connection_mut()?;
    connection
        .query_row(
            "SELECT candidate_id, repository_binding_id, branch_name, created_at \
             FROM candidate_created_branches WHERE candidate_id = ?1",
            params![candidate_id],
            |row| {
                Ok(CreatedBranchRecord {
                    candidate_id: row.get(0)?,
                    repository_binding_id: row.get(1)?,
                    branch_name: row.get(2)?,
                    created_at: row.get(3)?,
                })
            },
        )
        .optional()
        .map_err(crate::store::sql_error)
        .map_err(CandidateBranchError::from)
}

fn write_branch_record(
    store: &mut DeviceStore,
    candidate_id: &str,
    repository_binding_id: &str,
    branch_name: &str,
    requested_at: &str,
) -> Result<CreatedBranchRecord, CandidateBranchError> {
    ensure_branch_schema(store)?;
    let connection = store.connection_mut()?;
    connection
        .execute(
            "INSERT OR IGNORE INTO candidate_created_branches \
             (candidate_id, repository_binding_id, branch_name, created_at) \
             VALUES (?1, ?2, ?3, ?4)",
            params![
                candidate_id,
                repository_binding_id,
                branch_name,
                requested_at
            ],
        )
        .map_err(crate::store::sql_error)?;
    let stored = created_branch_record(store, candidate_id)?.ok_or_else(|| {
        CandidateBranchError::store("the branch record disappeared before the read-back")
    })?;
    if stored.repository_binding_id != repository_binding_id || stored.branch_name != branch_name {
        return Err(CandidateBranchError::conflict(format!(
            "candidate {candidate_id} already recorded branch {} on binding {}",
            stored.branch_name, stored.repository_binding_id
        )));
    }
    Ok(stored)
}

fn ensure_branch_schema(store: &mut DeviceStore) -> Result<(), CandidateBranchError> {
    store
        .connection_mut()?
        .execute_batch(
            "CREATE TABLE IF NOT EXISTS candidate_created_branches (
                candidate_id TEXT PRIMARY KEY NOT NULL,
                repository_binding_id TEXT NOT NULL,
                branch_name TEXT NOT NULL,
                created_at TEXT NOT NULL
            );",
        )
        .map_err(crate::store::sql_error)?;
    Ok(())
}

fn bound_repository_path(
    store: &mut DeviceStore,
    repository_binding_id: &str,
) -> Result<String, CandidateBranchError> {
    let mapping = store.path_mapping(repository_binding_id)?.ok_or_else(|| {
        CandidateBranchError::candidate_missing(format!(
            "repository binding {repository_binding_id} maps no local checkout"
        ))
    })?;
    if !Path::new(&mapping.canonical_path).exists() {
        return Err(CandidateBranchError::candidate_missing(format!(
            "repository binding {repository_binding_id} maps a checkout that is absent"
        )));
    }
    Ok(mapping.canonical_path)
}

fn derive_branch_name(
    repository_path: &str,
    task_slug: &str,
    candidate_commit: &str,
) -> Result<(String, bool), CandidateBranchError> {
    validate_task_slug(task_slug)?;
    for rung in SHORT_ID_RUNGS {
        let short_id = match rung {
            usize::MAX => candidate_commit,
            length => &candidate_commit[..length],
        };
        let name = format!("{WINWINCODE_BRANCH_PREFIX}{task_slug}-{short_id}");
        if validate_branch_name(&name).is_err() {
            continue;
        }
        match read_git_identity(repository_path, &branch_ref_name(&name))? {
            None => return Ok((name, false)),
            Some(commit) if commit == candidate_commit => return Ok((name, true)),
            Some(_) => {}
        }
    }
    Err(CandidateBranchError::conflict(format!(
        "every deterministic branch name for slug {task_slug} is occupied by another commit"
    )))
}

fn validate_request(request: &BranchCreationRequest) -> Result<(), CandidateBranchError> {
    require_non_empty(&request.repository_binding_id, "repository binding id")?;
    validate_candidate_ref(&request.candidate_ref)?;
    require_non_empty(&request.requested_at, "requested at")?;
    if let Some(name) = &request.requested_branch_name {
        validate_branch_name(name)?;
    }
    Ok(())
}

fn candidate_id_of(candidate_ref: &str) -> String {
    candidate_ref
        .strip_prefix(CANDIDATE_REF_PREFIX)
        .unwrap_or(candidate_ref)
        .to_owned()
}

fn validate_candidate_ref(candidate_ref: &str) -> Result<(), CandidateBranchError> {
    let Some(suffix) = candidate_ref.strip_prefix(CANDIDATE_REF_PREFIX) else {
        return Err(CandidateBranchError::invalid(format!(
            "candidate ref is not inside the {CANDIDATE_REF_PREFIX} namespace"
        )));
    };
    let valid = (suffix.len() == 40 || suffix.len() == 64)
        && suffix
            .bytes()
            .all(|byte| matches!(byte, b'0'..=b'9' | b'a'..=b'f'));
    if valid {
        Ok(())
    } else {
        Err(CandidateBranchError::invalid(
            "candidate ref does not name a full lowercase git commit id",
        ))
    }
}

fn validate_branch_name(name: &str) -> Result<(), CandidateBranchError> {
    let Some(suffix) = name.strip_prefix(WINWINCODE_BRANCH_PREFIX) else {
        return Err(CandidateBranchError::invalid(format!(
            "branch name is not inside the {WINWINCODE_BRANCH_PREFIX} namespace"
        )));
    };
    let shaped = !suffix.is_empty()
        && suffix.len() <= MAX_BRANCH_SUFFIX_BYTES
        && !suffix.contains("..")
        && !suffix.contains("@{")
        && !suffix.ends_with('.')
        && !suffix.ends_with('/')
        && suffix.split('/').all(|segment| {
            !segment.is_empty()
                && !segment.starts_with('.')
                && !segment.starts_with('-')
                && !segment.as_bytes().ends_with(b".lock")
                && segment
                    .bytes()
                    .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'_' | b'-'))
        });
    if shaped {
        Ok(())
    } else {
        Err(CandidateBranchError::invalid(
            "branch name is not a canonical winwincode branch name",
        ))
    }
}

fn validate_task_slug(task_slug: &str) -> Result<(), CandidateBranchError> {
    let valid = !task_slug.is_empty()
        && task_slug.len() <= MAX_SLUG_BYTES
        && task_slug
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'-'))
        && task_slug.as_bytes()[0].is_ascii_alphanumeric()
        && task_slug.as_bytes()[task_slug.len() - 1].is_ascii_alphanumeric();
    if valid {
        Ok(())
    } else {
        Err(CandidateBranchError::invalid(
            "task slug is not a canonical single-segment slug",
        ))
    }
}

fn require_non_empty(value: &str, label: &str) -> Result<(), CandidateBranchError> {
    if value.is_empty() {
        return Err(CandidateBranchError::invalid(format!(
            "{label} must not be empty"
        )));
    }
    if value.len() > MAX_ID_BYTES {
        return Err(CandidateBranchError::invalid(format!(
            "{label} must contain at most {MAX_ID_BYTES} bytes"
        )));
    }
    Ok(())
}

fn branch_ref_name(branch_name: &str) -> String {
    format!("refs/heads/{branch_name}")
}

fn read_git_identity(
    repository_path: &str,
    ref_name: &str,
) -> Result<Option<String>, CandidateBranchError> {
    let mut command = git_command(repository_path);
    command.args([
        "rev-parse",
        "--verify",
        "--quiet",
        "--end-of-options",
        ref_name,
    ]);
    let output = command
        .output()
        .map_err(|error| CandidateBranchError::store(format!("Git cannot be run: {error}")))?;
    if output.status.success() {
        let text = std::str::from_utf8(&output.stdout)
            .map_err(|_| CandidateBranchError::invalid("Git ref output is not UTF-8"))?;
        let text = text.trim_end_matches(['\r', '\n']);
        if text.is_empty() || text.contains(['\r', '\n']) {
            return Err(CandidateBranchError::store(
                "Git ref output is not a single identity",
            ));
        }
        return Ok(Some(text.to_owned()));
    }
    if output.status.code() == Some(1) {
        return Ok(None);
    }
    let stderr = String::from_utf8_lossy(&output.stderr);
    Err(CandidateBranchError::store(format!(
        "Git ref cannot be read: {}",
        stderr.trim()
    )))
}

fn create_git_branch(
    repository_path: &str,
    branch_name: &str,
    candidate_commit: &str,
) -> Result<(), CandidateBranchError> {
    let mut command = git_command(repository_path);
    command.args(["branch", "--end-of-options"]);
    command.arg(branch_name).arg(candidate_commit);
    let output = command
        .output()
        .map_err(|error| CandidateBranchError::store(format!("Git cannot be run: {error}")))?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        let stderr = stderr.trim();
        if stderr.contains("already exists") {
            return Err(CandidateBranchError::conflict(format!(
                "the branch {branch_name} already exists pointing at another commit"
            )));
        }
        return Err(CandidateBranchError::store(format!(
            "Git branch cannot be created: {stderr}"
        )));
    }
    let confirmed = read_git_identity(repository_path, &branch_ref_name(branch_name))?;
    if confirmed.as_deref() != Some(candidate_commit) {
        return Err(CandidateBranchError::store(format!(
            "the created branch {branch_name} does not resolve to the candidate commit"
        )));
    }
    Ok(())
}

fn git_command(repository_path: &str) -> Command {
    let mut command = Command::new("git");
    command.arg("-C").arg(repository_path);
    command.env("GIT_CONFIG_NOSYSTEM", "1");
    command.env("GIT_TERMINAL_PROMPT", "0");
    command
}

#[cfg(test)]
mod tests {
    use super::*;

    const COMMIT: &str = "0f9e8d7c6b5a4938271605f4e3d2c1b0a9988776";

    #[test]
    fn a_canonical_winwincode_branch_name_validates() {
        assert_eq!(
            validate_branch_name("winwincode/fix-login"),
            Ok(()),
            "a plain namespace branch validates"
        );
        assert_eq!(
            validate_branch_name("winwincode/task/sub_task-v1.2"),
            Ok(()),
            "nested segments with dot, underscore, and dash validate"
        );
    }

    #[test]
    fn branch_names_outside_the_vocabulary_are_rejected() {
        for (label, name) in [
            ("a user branch", "main"),
            ("an empty suffix", "winwincode/"),
            ("outside the namespace", "feature/x"),
            ("a dot segment", "winwincode/.hidden"),
            ("a dash-led segment", "winwincode/-flag"),
            ("a lock suffix", "winwincode/task.lock"),
            ("a dot-dot walk", "winwincode/../escape"),
            ("a reflog walk", "winwincode/task@{{x}}"),
            ("a trailing dot", "winwincode/task."),
            ("a trailing slash", "winwincode/task/"),
            ("an empty segment", "winwincode/task//more"),
            ("a space", "winwincode/fix login"),
            (
                "an overlong name",
                &format!("winwincode/{}", "a".repeat(161)),
            ),
        ] {
            assert_eq!(
                validate_branch_name(name).unwrap_err().kind(),
                CandidateBranchErrorKind::InvalidInput,
                "{label} must fail closed"
            );
        }
    }

    #[test]
    fn slugs_outside_the_vocabulary_are_rejected() {
        for (label, slug) in [
            ("empty", ""),
            ("an overlong slug", &"a".repeat(MAX_SLUG_BYTES + 1)),
            ("a path walk", "../evil"),
            ("a namespace escape", "a/b"),
            ("a leading dash", "-task"),
            ("a trailing dash", "task-"),
            ("a dot", "task."),
            ("a space", "fix login"),
        ] {
            assert_eq!(
                validate_task_slug(slug).unwrap_err().kind(),
                CandidateBranchErrorKind::InvalidInput,
                "{label} must fail closed"
            );
        }
        assert_eq!(validate_task_slug("fix_login-2"), Ok(()));
    }

    #[test]
    fn candidate_refs_outside_the_frozen_shape_are_rejected() {
        assert_eq!(
            validate_candidate_ref(&format!("{CANDIDATE_REF_PREFIX}{COMMIT}")),
            Ok(())
        );
        for (label, reference) in [
            ("a user branch", "refs/heads/main"),
            (
                "an abbreviated commit",
                &format!("{CANDIDATE_REF_PREFIX}0f9e8d7c"),
            ),
            (
                "an uppercase commit",
                &format!("{CANDIDATE_REF_PREFIX}{}", COMMIT.to_uppercase()),
            ),
        ] {
            assert_eq!(
                validate_candidate_ref(reference).unwrap_err().kind(),
                CandidateBranchErrorKind::InvalidInput,
                "{label} must fail closed"
            );
        }
        assert_eq!(
            candidate_id_of(&format!("{CANDIDATE_REF_PREFIX}{COMMIT}")),
            COMMIT
        );
    }

    #[test]
    fn the_branch_ref_names_the_heads_namespace() {
        assert_eq!(
            branch_ref_name("winwincode/fix-login"),
            "refs/heads/winwincode/fix-login"
        );
    }
}
