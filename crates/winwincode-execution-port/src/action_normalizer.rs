// SPDX-License-Identifier: Apache-2.0

//! Pure action-intent validation and tool-request normalization.
//!
//! This module deliberately has no gate, tool runner, filesystem access, process
//! spawning, or network access.  Callers pass the request which is about to be
//! executed and receive stable data that a gate or trace adapter can consume.

use std::collections::BTreeSet;
use std::fmt;

use serde::{Deserialize, Serialize};
use serde_json::Value;

/// Version of the frozen JSON shape emitted by this module.
pub const ACTION_NORMALIZATION_SCHEMA_VERSION: &str = "winwincode.action-normalization.v1";

/// Object named by an executor intent or inferred from a real tool request.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ActionObject {
    ProductionCode,
    Test,
    Config,
    Dependency,
    Schema,
    Ci,
    ExternalResource,
}

impl ActionObject {
    fn as_str(self) -> &'static str {
        match self {
            Self::ProductionCode => "production_code",
            Self::Test => "test",
            Self::Config => "config",
            Self::Dependency => "dependency",
            Self::Schema => "schema",
            Self::Ci => "ci",
            Self::ExternalResource => "external_resource",
        }
    }
}

/// Side effect named by an intent or inferred from a real tool request.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ActionOperation {
    Create,
    Modify,
    Delete,
    Execute,
    Deploy,
}

impl ActionOperation {
    fn as_str(self) -> &'static str {
        match self {
            Self::Create => "create",
            Self::Modify => "modify",
            Self::Delete => "delete",
            Self::Execute => "execute",
            Self::Deploy => "deploy",
        }
    }
}

/// Executor purpose. It remains self-reported and never changes normalization.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ActionPurpose {
    Diagnose,
    Implement,
    Repair,
    Refactor,
    Verify,
    Migrate,
}

/// Maximum radius declared by the executor or inferred from actual targets.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ActionScope {
    Local,
    Module,
    CrossModule,
    Repository,
    External,
}

impl ActionScope {
    fn as_str(self) -> &'static str {
        match self {
            Self::Local => "local",
            Self::Module => "module",
            Self::CrossModule => "cross_module",
            Self::Repository => "repository",
            Self::External => "external",
        }
    }
}

/// Executor risk estimate. The normalizer computes its own minimum level.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ActionRisk {
    Low,
    Medium,
    High,
    Critical,
}

impl ActionRisk {
    fn as_str(self) -> &'static str {
        match self {
            Self::Low => "low",
            Self::Medium => "medium",
            Self::High => "high",
            Self::Critical => "critical",
        }
    }
}

/// Structured executor statement submitted before a critical side effect.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ActionIntent {
    pub object: ActionObject,
    pub operation: ActionOperation,
    pub intent: ActionPurpose,
    pub scope: ActionScope,
    pub targets: Vec<String>,
    pub requirement_refs: Vec<String>,
    pub plan_refs: Vec<String>,
    pub expected_effect: String,
    pub scope_delta: Option<String>,
    pub rollback: Option<String>,
    pub executor_risk: ActionRisk,
}

/// File operation presented to the capability gateway.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FileOperation {
    Create,
    Write,
    Delete,
    Execute,
}

/// Test ownership supplied by the trusted test-asset inventory.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TestOwner {
    RepositoryExisting,
    ExecutorWorking,
    CanonicalAcceptance,
    ProtectedAdversarial,
}

/// Assertion effect supplied by a deterministic AST or diff adapter.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AssertionEffect {
    #[default]
    Unchanged,
    Strengthened,
    Weakened,
    Unknown,
}

/// Dependency effect supplied by a package-manifest or lockfile adapter.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DependencyEffect {
    #[default]
    None,
    Added,
    Updated,
    Removed,
}

/// Trusted static facts computed before the file operation reaches this module.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct FileAnalysis {
    #[serde(default)]
    pub test_owner: Option<TestOwner>,
    #[serde(default)]
    pub assertion_effect: AssertionEffect,
    #[serde(default)]
    pub dependency_effect: DependencyEffect,
    #[serde(default)]
    pub public_api_change: bool,
    #[serde(default)]
    pub migration_change: bool,
}

/// One real file request. Paths are workspace-relative unless absolute paths are explicit.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct FileRequest {
    pub operation: FileOperation,
    pub paths: Vec<String>,
    #[serde(default)]
    pub analysis: FileAnalysis,
}

/// Git operation presented to the capability gateway.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum GitOperation {
    Status,
    Diff,
    Show,
    Stage,
    Commit,
    BranchCreate,
    Merge,
    Rebase,
    Reset,
    Clean,
    Fetch,
    Pull,
    Push,
    Clone,
}

/// One real Git request.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct GitRequest {
    pub operation: GitOperation,
    pub repository_path: String,
    #[serde(default)]
    pub refs: Vec<String>,
}

/// One real argv-based shell request. No shell string is reparsed or executed here.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ShellRequest {
    pub program: String,
    #[serde(default)]
    pub args: Vec<String>,
    pub working_directory: String,
}

/// One real HTTP network request. Query data is never copied into normalized targets.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct NetworkRequest {
    pub method: String,
    pub url: String,
}

/// One real MCP capability request.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct McpRequest {
    pub server: String,
    pub tool: String,
    #[serde(default)]
    pub arguments: Value,
}

/// Supported gateway request families.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "source", rename_all = "snake_case", deny_unknown_fields)]
pub enum ToolRequest {
    File(FileRequest),
    Git(GitRequest),
    Shell(ShellRequest),
    Network(NetworkRequest),
    Mcp(McpRequest),
}

/// Gateway source which produced the observed action.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ActionSource {
    File,
    Git,
    Shell,
    Network,
    Mcp,
}

/// Stable evidence used to derive classification and risk.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ObservedFact {
    FilePath,
    TestPath,
    DependencyManifest,
    DependencyLockfile,
    ConfigPath,
    SchemaPath,
    CiPath,
    CanonicalTest,
    ProtectedTest,
    AssertionStrengthened,
    AssertionWeakened,
    AssertionEffectUnknown,
    DependencyAdded,
    DependencyUpdated,
    DependencyRemoved,
    PublicApiChanged,
    MigrationChanged,
    GitRepository,
    GitRemoteRead,
    GitRemoteWrite,
    ShellCommand,
    NetworkRead,
    NetworkWrite,
    McpCapability,
}

/// Deterministically classified action derived from the actual request.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ObservedAction {
    pub source: ActionSource,
    pub objects: Vec<ActionObject>,
    pub operation: ActionOperation,
    pub scope: ActionScope,
    pub targets: Vec<String>,
    pub facts: Vec<ObservedFact>,
    pub minimum_risk: ActionRisk,
}

/// Exact field whose observed value does not fit the executor intent.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum IntentMismatchKind {
    Object,
    Operation,
    Scope,
    Target,
    Risk,
}

/// One stable, explainable intent mismatch.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct IntentMismatch {
    pub kind: IntentMismatchKind,
    pub declared: String,
    pub observed: String,
    pub explanation: String,
}

/// Ordered comparison between self-reported intent and the normalized request.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct IntentComparison {
    pub matches: bool,
    pub mismatches: Vec<IntentMismatch>,
}

/// Complete pure output ready for a gate or trace adapter.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ActionNormalization {
    pub schema_version: String,
    pub intent: ActionIntent,
    pub observed: ObservedAction,
    pub comparison: IntentComparison,
}

/// Deterministic validation failure. It never includes secret-bearing request arguments.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ActionNormalizationError {
    pub code: ActionNormalizationErrorCode,
    pub field: String,
    pub message: String,
}

/// Stable validation error category.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ActionNormalizationErrorCode {
    Empty,
    Duplicate,
    InvalidPath,
    InvalidReference,
    InvalidMethod,
    InvalidUrl,
    InvalidMcpIdentifier,
}

impl fmt::Display for ActionNormalizationError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "{}: {}", self.field, self.message)
    }
}

impl std::error::Error for ActionNormalizationError {}

/// Validates and normalizes one pending tool request without performing it.
///
/// # Errors
///
/// Returns a stable validation error when the intent or typed tool request is
/// empty, malformed, duplicated, or escapes its supported lexical boundary.
pub fn normalize_action(
    intent: &ActionIntent,
    request: &ToolRequest,
) -> Result<ActionNormalization, ActionNormalizationError> {
    validate_intent(intent)?;
    let observed = normalize_request(request)?;
    let comparison = compare_intent(intent, &observed);
    Ok(ActionNormalization {
        schema_version: ACTION_NORMALIZATION_SCHEMA_VERSION.to_owned(),
        intent: intent.clone(),
        observed,
        comparison,
    })
}

/// Derives the trusted observed action from the exact typed tool request.
///
/// # Errors
///
/// Rejects malformed request fields before any gate or tool side effect.
pub fn observe_action(request: &ToolRequest) -> Result<ObservedAction, ActionNormalizationError> {
    normalize_request(request)
}

/// Compares a validated intent with one observed action in a fixed field order.
#[must_use]
pub fn compare_intent(intent: &ActionIntent, observed: &ObservedAction) -> IntentComparison {
    let mut mismatches = Vec::new();

    for object in &observed.objects {
        if *object != intent.object {
            mismatches.push(IntentMismatch {
                kind: IntentMismatchKind::Object,
                declared: intent.object.as_str().to_owned(),
                observed: object.as_str().to_owned(),
                explanation: format!(
                    "observed {} action is outside declared {} object",
                    object.as_str(),
                    intent.object.as_str()
                ),
            });
        }
    }

    if observed.operation != intent.operation {
        mismatches.push(IntentMismatch {
            kind: IntentMismatchKind::Operation,
            declared: intent.operation.as_str().to_owned(),
            observed: observed.operation.as_str().to_owned(),
            explanation: format!(
                "observed {} operation differs from declared {} operation",
                observed.operation.as_str(),
                intent.operation.as_str()
            ),
        });
    }

    if observed.scope > intent.scope {
        mismatches.push(IntentMismatch {
            kind: IntentMismatchKind::Scope,
            declared: intent.scope.as_str().to_owned(),
            observed: observed.scope.as_str().to_owned(),
            explanation: format!(
                "observed {} scope exceeds declared {} scope",
                observed.scope.as_str(),
                intent.scope.as_str()
            ),
        });
    }

    for target in &observed.targets {
        if !intent
            .targets
            .iter()
            .any(|declared| target_is_covered(declared, target))
        {
            mismatches.push(IntentMismatch {
                kind: IntentMismatchKind::Target,
                declared: intent.targets.join(","),
                observed: target.clone(),
                explanation: format!("observed target {target} is outside declared targets"),
            });
        }
    }

    if observed.minimum_risk > intent.executor_risk {
        mismatches.push(IntentMismatch {
            kind: IntentMismatchKind::Risk,
            declared: intent.executor_risk.as_str().to_owned(),
            observed: observed.minimum_risk.as_str().to_owned(),
            explanation: format!(
                "observed facts require at least {} risk, above declared {} risk",
                observed.minimum_risk.as_str(),
                intent.executor_risk.as_str()
            ),
        });
    }

    IntentComparison {
        matches: mismatches.is_empty(),
        mismatches,
    }
}

fn validate_intent(intent: &ActionIntent) -> Result<(), ActionNormalizationError> {
    validate_non_empty("intent.targets", &intent.targets)?;
    validate_unique("intent.targets", &intent.targets)?;
    validate_references("intent.requirementRefs", &intent.requirement_refs)?;
    validate_references("intent.planRefs", &intent.plan_refs)?;
    validate_text("intent.expectedEffect", &intent.expected_effect)?;
    if let Some(scope_delta) = &intent.scope_delta {
        validate_text("intent.scopeDelta", scope_delta)?;
    }
    if let Some(rollback) = &intent.rollback {
        validate_text("intent.rollback", rollback)?;
    }
    Ok(())
}

fn validate_non_empty(field: &str, values: &[String]) -> Result<(), ActionNormalizationError> {
    if values.is_empty() {
        return Err(error(
            ActionNormalizationErrorCode::Empty,
            field,
            "must not be empty",
        ));
    }
    for value in values {
        validate_text(field, value)?;
    }
    Ok(())
}

fn validate_unique(field: &str, values: &[String]) -> Result<(), ActionNormalizationError> {
    let mut unique = BTreeSet::new();
    for value in values {
        if !unique.insert(value) {
            return Err(error(
                ActionNormalizationErrorCode::Duplicate,
                field,
                "must not contain duplicates",
            ));
        }
    }
    Ok(())
}

fn validate_references(field: &str, values: &[String]) -> Result<(), ActionNormalizationError> {
    validate_unique(field, values)?;
    for value in values {
        validate_text(field, value)?;
        if value.bytes().any(|byte| byte.is_ascii_whitespace()) {
            return Err(error(
                ActionNormalizationErrorCode::InvalidReference,
                field,
                "must contain stable non-whitespace references",
            ));
        }
    }
    Ok(())
}

fn validate_text(field: &str, value: &str) -> Result<(), ActionNormalizationError> {
    if value.trim().is_empty() {
        return Err(error(
            ActionNormalizationErrorCode::Empty,
            field,
            "must not be blank",
        ));
    }
    if value.contains('\0') {
        return Err(error(
            ActionNormalizationErrorCode::Empty,
            field,
            "must not contain NUL",
        ));
    }
    Ok(())
}

fn normalize_request(request: &ToolRequest) -> Result<ObservedAction, ActionNormalizationError> {
    match request {
        ToolRequest::File(request) => normalize_file(request),
        ToolRequest::Git(request) => normalize_git(request),
        ToolRequest::Shell(request) => normalize_shell(request),
        ToolRequest::Network(request) => normalize_network(request),
        ToolRequest::Mcp(request) => normalize_mcp(request),
    }
}

fn normalize_file(request: &FileRequest) -> Result<ObservedAction, ActionNormalizationError> {
    validate_non_empty("request.paths", &request.paths)?;
    let mut targets = request
        .paths
        .iter()
        .map(|path| normalize_path("request.paths", path))
        .collect::<Result<Vec<_>, _>>()?;
    targets.sort();
    targets.dedup();

    let mut objects = BTreeSet::new();
    let mut facts = BTreeSet::from([ObservedFact::FilePath]);
    for path in &targets {
        let classification = classify_path(path);
        objects.insert(classification.object);
        facts.insert(classification.fact);
        if classification.lockfile {
            facts.insert(ObservedFact::DependencyLockfile);
        }
    }
    if request.analysis.test_owner.is_some() {
        objects.insert(ActionObject::Test);
        facts.insert(ObservedFact::TestPath);
    }
    match request.analysis.test_owner {
        Some(TestOwner::CanonicalAcceptance) => {
            facts.insert(ObservedFact::CanonicalTest);
        }
        Some(TestOwner::ProtectedAdversarial) => {
            facts.insert(ObservedFact::ProtectedTest);
        }
        _ => {}
    }
    match request.analysis.assertion_effect {
        AssertionEffect::Unchanged => {}
        AssertionEffect::Strengthened => {
            facts.insert(ObservedFact::AssertionStrengthened);
        }
        AssertionEffect::Weakened => {
            facts.insert(ObservedFact::AssertionWeakened);
        }
        AssertionEffect::Unknown => {
            facts.insert(ObservedFact::AssertionEffectUnknown);
        }
    }
    match request.analysis.dependency_effect {
        DependencyEffect::None => {}
        DependencyEffect::Added => {
            objects.insert(ActionObject::Dependency);
            facts.insert(ObservedFact::DependencyAdded);
        }
        DependencyEffect::Updated => {
            objects.insert(ActionObject::Dependency);
            facts.insert(ObservedFact::DependencyUpdated);
        }
        DependencyEffect::Removed => {
            objects.insert(ActionObject::Dependency);
            facts.insert(ObservedFact::DependencyRemoved);
        }
    }
    if request.analysis.public_api_change {
        facts.insert(ObservedFact::PublicApiChanged);
    }
    if request.analysis.migration_change {
        facts.insert(ObservedFact::MigrationChanged);
    }

    let operation = match request.operation {
        FileOperation::Create => ActionOperation::Create,
        FileOperation::Write => ActionOperation::Modify,
        FileOperation::Delete => ActionOperation::Delete,
        FileOperation::Execute => ActionOperation::Execute,
    };
    let scope = file_scope(&targets);
    let facts = facts.into_iter().collect::<Vec<_>>();
    Ok(ObservedAction {
        source: ActionSource::File,
        objects: objects.into_iter().collect(),
        operation,
        scope,
        targets,
        minimum_risk: minimum_risk(operation, &facts),
        facts,
    })
}

fn normalize_git(request: &GitRequest) -> Result<ObservedAction, ActionNormalizationError> {
    let repository = normalize_path("request.repositoryPath", &request.repository_path)?;
    validate_unique("request.refs", &request.refs)?;
    for reference in &request.refs {
        validate_text("request.refs", reference)?;
    }
    let mut targets = vec![repository];
    let mut refs = request.refs.clone();
    refs.sort();
    targets.extend(
        refs.into_iter()
            .map(|reference| format!("git-ref:{reference}")),
    );

    let mut facts = BTreeSet::from([ObservedFact::GitRepository]);
    let operation = match request.operation {
        GitOperation::Status | GitOperation::Diff | GitOperation::Show => ActionOperation::Execute,
        GitOperation::BranchCreate | GitOperation::Clone => ActionOperation::Create,
        GitOperation::Clean => ActionOperation::Delete,
        GitOperation::Push => {
            facts.insert(ObservedFact::GitRemoteWrite);
            ActionOperation::Deploy
        }
        GitOperation::Fetch | GitOperation::Pull => {
            facts.insert(ObservedFact::GitRemoteRead);
            ActionOperation::Modify
        }
        GitOperation::Stage
        | GitOperation::Commit
        | GitOperation::Merge
        | GitOperation::Rebase
        | GitOperation::Reset => ActionOperation::Modify,
    };
    let facts = facts.into_iter().collect::<Vec<_>>();
    Ok(ObservedAction {
        source: ActionSource::Git,
        objects: vec![ActionObject::ProductionCode],
        operation,
        scope: ActionScope::Repository,
        targets,
        minimum_risk: minimum_risk(operation, &facts),
        facts,
    })
}

fn normalize_shell(request: &ShellRequest) -> Result<ObservedAction, ActionNormalizationError> {
    validate_text("request.program", &request.program)?;
    validate_non_blank_values("request.args", &request.args)?;
    let working_directory = normalize_path("request.workingDirectory", &request.working_directory)?;
    let program = basename(&request.program).to_ascii_lowercase();
    let args = request.args.iter().map(String::as_str).collect::<Vec<_>>();
    let (object, operation, extra_fact) = classify_shell(&program, &args);

    let command = serde_json::to_string(
        &std::iter::once(request.program.as_str())
            .chain(args.iter().copied())
            .collect::<Vec<_>>(),
    )
    .map_err(|_| {
        error(
            ActionNormalizationErrorCode::Empty,
            "request",
            "cannot encode argv",
        )
    })?;
    let mut facts = BTreeSet::from([ObservedFact::ShellCommand]);
    if let Some(fact) = extra_fact {
        facts.insert(fact);
    }
    let facts = facts.into_iter().collect::<Vec<_>>();
    Ok(ObservedAction {
        source: ActionSource::Shell,
        objects: vec![object],
        operation,
        scope: shell_scope(&working_directory),
        targets: vec![
            format!("cwd:{working_directory}"),
            format!("argv:{command}"),
        ],
        minimum_risk: minimum_risk(operation, &facts),
        facts,
    })
}

fn normalize_network(request: &NetworkRequest) -> Result<ObservedAction, ActionNormalizationError> {
    let method = request.method.trim().to_ascii_uppercase();
    if method.is_empty()
        || !method
            .bytes()
            .all(|byte| byte.is_ascii_uppercase() || byte == b'-')
    {
        return Err(error(
            ActionNormalizationErrorCode::InvalidMethod,
            "request.method",
            "must be an HTTP token",
        ));
    }
    let endpoint = normalize_url(&request.url)?;
    let (operation, fact) = match method.as_str() {
        "GET" | "HEAD" | "OPTIONS" => (ActionOperation::Execute, ObservedFact::NetworkRead),
        "DELETE" => (ActionOperation::Delete, ObservedFact::NetworkWrite),
        "POST" => (ActionOperation::Create, ObservedFact::NetworkWrite),
        "PATCH" | "PUT" => (ActionOperation::Modify, ObservedFact::NetworkWrite),
        _ => (ActionOperation::Execute, ObservedFact::NetworkWrite),
    };
    let facts = vec![fact];
    Ok(ObservedAction {
        source: ActionSource::Network,
        objects: vec![ActionObject::ExternalResource],
        operation,
        scope: ActionScope::External,
        targets: vec![format!("{method} {endpoint}")],
        minimum_risk: minimum_risk(operation, &facts),
        facts,
    })
}

fn normalize_mcp(request: &McpRequest) -> Result<ObservedAction, ActionNormalizationError> {
    let capability_id = canonical_mcp_capability_id(&request.server, &request.tool)?;
    let tool = request.tool.to_ascii_lowercase();
    let operation = classify_named_operation(&tool);
    let facts = vec![ObservedFact::McpCapability];
    // Arguments are intentionally not copied into targets or validation errors: the
    // raw request remains available to the gateway, while normalized trace data does
    // not accidentally turn an MCP credential argument into a policy identifier.
    Ok(ObservedAction {
        source: ActionSource::Mcp,
        objects: vec![ActionObject::ExternalResource],
        operation,
        scope: ActionScope::External,
        targets: vec![capability_id],
        minimum_risk: minimum_risk(operation, &facts),
        facts,
    })
}

/// Returns the secret-free canonical capability identifier for one MCP target.
///
/// The identifier contains only the validated MCP server and tool names. MCP
/// arguments, connection configuration, and credentials are never included.
///
/// # Errors
///
/// Returns an invalid-MCP-identifier error when either name is blank or uses
/// characters outside the portable MCP identifier alphabet.
pub fn canonical_mcp_capability_id(
    server: &str,
    tool: &str,
) -> Result<String, ActionNormalizationError> {
    validate_mcp_identifier("request.server", server)?;
    validate_mcp_identifier("request.tool", tool)?;
    Ok(format!(
        "mcp://{}/{}",
        server.to_ascii_lowercase(),
        tool.to_ascii_lowercase()
    ))
}

struct PathClassification {
    object: ActionObject,
    fact: ObservedFact,
    lockfile: bool,
}

fn classify_path(path: &str) -> PathClassification {
    let lower = path.to_ascii_lowercase();
    let file_name = lower.rsplit('/').next().unwrap_or(lower.as_str());
    let components = lower.split('/').collect::<Vec<_>>();
    let is_test = components.iter().any(|component| {
        matches!(
            *component,
            "test" | "tests" | "__tests__" | "spec" | "specs"
        )
    }) || file_name.contains(".test.")
        || file_name.contains(".spec.")
        || file_name.ends_with("_test.rs")
        || file_name.starts_with("test_");
    if is_test {
        return PathClassification {
            object: ActionObject::Test,
            fact: ObservedFact::TestPath,
            lockfile: false,
        };
    }
    let dependency_manifest = matches!(
        file_name,
        "cargo.toml"
            | "package.json"
            | "deno.json"
            | "deno.jsonc"
            | "pyproject.toml"
            | "requirements.txt"
            | "go.mod"
            | "go.sum"
            | "pom.xml"
            | "build.gradle"
            | "build.gradle.kts"
    );
    let dependency_lockfile = matches!(
        file_name,
        "cargo.lock"
            | "pnpm-lock.yaml"
            | "package-lock.json"
            | "yarn.lock"
            | "bun.lock"
            | "bun.lockb"
            | "poetry.lock"
            | "uv.lock"
    );
    if dependency_manifest || dependency_lockfile {
        return PathClassification {
            object: ActionObject::Dependency,
            fact: ObservedFact::DependencyManifest,
            lockfile: dependency_lockfile,
        };
    }
    if components.starts_with(&[".github", "workflows"])
        || components.contains(&".gitlab-ci")
        || matches!(file_name, ".gitlab-ci.yml" | "jenkinsfile")
    {
        return PathClassification {
            object: ActionObject::Ci,
            fact: ObservedFact::CiPath,
            lockfile: false,
        };
    }
    if components.contains(&"schema")
        || file_name.contains(".schema.")
        || matches!(file_name, "openapi.json" | "openapi.yaml" | "openapi.yml")
    {
        return PathClassification {
            object: ActionObject::Schema,
            fact: ObservedFact::SchemaPath,
            lockfile: false,
        };
    }
    if (components.contains(&"config") || components.contains(&"configs"))
        || file_name.starts_with('.')
        || matches!(
            file_name.rsplit('.').next(),
            Some("toml" | "yaml" | "yml" | "ini" | "conf")
        )
    {
        return PathClassification {
            object: ActionObject::Config,
            fact: ObservedFact::ConfigPath,
            lockfile: false,
        };
    }
    PathClassification {
        object: ActionObject::ProductionCode,
        fact: ObservedFact::FilePath,
        lockfile: false,
    }
}

fn classify_shell(
    program: &str,
    args: &[&str],
) -> (ActionObject, ActionOperation, Option<ObservedFact>) {
    let first = args.first().copied().unwrap_or("");
    let is_destructive = matches!(
        program,
        "del" | "erase" | "mkfs" | "rm" | "rmdir" | "shred" | "unlink"
    ) || program == "git" && matches!(first, "clean" | "reset");
    if is_destructive {
        return (ActionObject::ProductionCode, ActionOperation::Delete, None);
    }
    let is_test = matches!(program, "pytest" | "jest" | "vitest" | "mocha" | "ctest")
        || program == "cargo" && first == "test"
        || matches!(program, "pnpm" | "npm" | "yarn" | "bun")
            && (first == "test" || args.windows(2).any(|pair| pair == ["run", "test"]));
    if is_test {
        return (
            ActionObject::Test,
            ActionOperation::Execute,
            Some(ObservedFact::TestPath),
        );
    }
    let is_dependency = program == "cargo" && matches!(first, "add" | "remove" | "update")
        || matches!(program, "pnpm" | "npm" | "yarn" | "bun")
            && matches!(first, "add" | "install" | "remove" | "uninstall" | "update")
        || matches!(program, "pip" | "pip3" | "uv" | "poetry")
            && matches!(first, "add" | "install" | "remove" | "uninstall" | "update");
    if is_dependency {
        return (
            ActionObject::Dependency,
            ActionOperation::Modify,
            Some(ObservedFact::DependencyUpdated),
        );
    }
    (ActionObject::ProductionCode, ActionOperation::Execute, None)
}

fn classify_named_operation(name: &str) -> ActionOperation {
    let verb = name
        .split(['.', '/', ':', '-', '_'])
        .find(|component| !component.is_empty())
        .unwrap_or(name);
    match verb {
        "create" | "add" | "insert" => ActionOperation::Create,
        "update" | "write" | "edit" | "set" | "patch" => ActionOperation::Modify,
        "delete" | "remove" | "destroy" => ActionOperation::Delete,
        "deploy" | "publish" | "release" | "push" => ActionOperation::Deploy,
        _ => ActionOperation::Execute,
    }
}

fn minimum_risk(operation: ActionOperation, facts: &[ObservedFact]) -> ActionRisk {
    if facts.iter().any(|fact| {
        matches!(
            fact,
            ObservedFact::CanonicalTest
                | ObservedFact::ProtectedTest
                | ObservedFact::AssertionWeakened
        )
    }) {
        return ActionRisk::Critical;
    }
    if matches!(operation, ActionOperation::Delete | ActionOperation::Deploy)
        || facts.iter().any(|fact| {
            matches!(
                fact,
                ObservedFact::DependencyAdded
                    | ObservedFact::DependencyRemoved
                    | ObservedFact::PublicApiChanged
                    | ObservedFact::MigrationChanged
                    | ObservedFact::GitRemoteWrite
                    | ObservedFact::NetworkWrite
            )
        })
    {
        return ActionRisk::High;
    }
    if operation == ActionOperation::Modify
        || facts.iter().any(|fact| {
            matches!(
                fact,
                ObservedFact::DependencyUpdated | ObservedFact::AssertionEffectUnknown
            )
        })
    {
        return ActionRisk::Medium;
    }
    ActionRisk::Low
}

fn file_scope(paths: &[String]) -> ActionScope {
    if paths.iter().any(|path| {
        !path.contains('/')
            || matches!(
                classify_path(path).object,
                ActionObject::Dependency | ActionObject::Ci
            )
    }) {
        return ActionScope::Repository;
    }
    let modules = paths
        .iter()
        .filter_map(|path| path.trim_start_matches('/').split('/').next())
        .collect::<BTreeSet<_>>();
    if paths.len() == 1 {
        ActionScope::Local
    } else if modules.len() == 1 {
        ActionScope::Module
    } else {
        ActionScope::CrossModule
    }
}

fn shell_scope(working_directory: &str) -> ActionScope {
    if working_directory == "." || !working_directory.contains('/') {
        ActionScope::Repository
    } else {
        ActionScope::Module
    }
}

fn normalize_path(field: &str, value: &str) -> Result<String, ActionNormalizationError> {
    validate_text(field, value)?;
    let replaced = value.replace('\\', "/");
    let absolute = replaced.starts_with('/');
    let mut components = Vec::new();
    for component in replaced.split('/') {
        match component {
            "" | "." => {}
            ".." => {
                if components.pop().is_none() {
                    return Err(error(
                        ActionNormalizationErrorCode::InvalidPath,
                        field,
                        "must not escape its root",
                    ));
                }
            }
            value => components.push(value),
        }
    }
    if components.is_empty() {
        return Ok(if absolute { "/" } else { "." }.to_owned());
    }
    let joined = components.join("/");
    Ok(if absolute {
        format!("/{joined}")
    } else {
        joined
    })
}

fn normalize_url(value: &str) -> Result<String, ActionNormalizationError> {
    validate_text("request.url", value)?;
    let (scheme, remainder) = value.split_once("://").ok_or_else(|| {
        error(
            ActionNormalizationErrorCode::InvalidUrl,
            "request.url",
            "must be an absolute HTTP(S) URL",
        )
    })?;
    let scheme = scheme.to_ascii_lowercase();
    if !matches!(scheme.as_str(), "http" | "https") {
        return Err(error(
            ActionNormalizationErrorCode::InvalidUrl,
            "request.url",
            "must use http or https",
        ));
    }
    let authority_end = remainder.find(['/', '?', '#']).unwrap_or(remainder.len());
    let authority = &remainder[..authority_end];
    if authority.is_empty() || authority.contains('@') {
        return Err(error(
            ActionNormalizationErrorCode::InvalidUrl,
            "request.url",
            "must contain a host and no credentials",
        ));
    }
    let mut authority = authority.to_ascii_lowercase();
    if scheme == "http" && authority.ends_with(":80") {
        authority.truncate(authority.len() - 3);
    } else if scheme == "https" && authority.ends_with(":443") {
        authority.truncate(authority.len() - 4);
    }
    let suffix = &remainder[authority_end..];
    let path_end = suffix.find(['?', '#']).unwrap_or(suffix.len());
    let path = &suffix[..path_end];
    let path = if path.is_empty() { "/" } else { path };
    Ok(format!("{scheme}://{authority}{path}"))
}

fn validate_mcp_identifier(field: &str, value: &str) -> Result<(), ActionNormalizationError> {
    validate_text(field, value)?;
    if !value
        .bytes()
        .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'_' | b'-' | b'/'))
    {
        return Err(error(
            ActionNormalizationErrorCode::InvalidMcpIdentifier,
            field,
            "must contain only portable identifier characters",
        ));
    }
    Ok(())
}

fn validate_non_blank_values(
    field: &str,
    values: &[String],
) -> Result<(), ActionNormalizationError> {
    for value in values {
        validate_text(field, value)?;
    }
    Ok(())
}

fn target_is_covered(declared: &str, observed: &str) -> bool {
    let declared = declared.trim().trim_end_matches('/');
    declared == "*"
        || declared == observed
        || observed
            .strip_prefix(declared)
            .is_some_and(|suffix| suffix.starts_with('/') || suffix.starts_with(':'))
}

fn basename(value: &str) -> &str {
    value.rsplit(['/', '\\']).next().unwrap_or(value)
}

fn error(
    code: ActionNormalizationErrorCode,
    field: &str,
    message: &str,
) -> ActionNormalizationError {
    ActionNormalizationError {
        code,
        field: field.to_owned(),
        message: message.to_owned(),
    }
}
