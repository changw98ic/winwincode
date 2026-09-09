// SPDX-License-Identifier: Apache-2.0

//! Versioned development recipes and deterministic automation occurrences.

use std::collections::{BTreeMap, BTreeSet};
use std::fmt;
use std::str::FromStr as _;

use jiff::{SignedDuration, Timestamp, civil::DateTime, tz::TimeZone};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

const SCHEMA_VERSION: u8 = 1;
const MAX_IMPORT_BYTES: usize = 256 * 1024;
const MIN_RANKING_SAMPLE: u32 = 5;

/// The six supported development recipe families.
#[derive(Clone, Copy, Debug, Deserialize, Eq, Ord, PartialEq, PartialOrd, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum DevelopmentRecipeKind {
    BugFix,
    ApiDevelopment,
    PageDevelopment,
    TestCoverage,
    DependencyUpgrade,
    Refactor,
}

/// Closed risk level used by recipes.
#[derive(Clone, Copy, Debug, Deserialize, Eq, Ord, PartialEq, PartialOrd, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum RecipeRiskLevel {
    Low,
    Medium,
    High,
}

/// Minimum gate required before a resolved task may execute.
#[derive(Clone, Copy, Debug, Deserialize, Eq, Ord, PartialEq, PartialOrd, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum RecipeGate {
    Standard,
    Review,
    HumanApproval,
}

/// Exact versioned skill or verification-pack reference.
#[derive(Clone, Debug, Deserialize, Eq, Ord, PartialEq, PartialOrd, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct RecipeReference {
    pub id: String,
    pub version: u32,
}

/// Parameter kind. Credentials must remain references and never defaults.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum RecipeParameterKind {
    Text,
    CredentialReference,
}

/// One bounded recipe input.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct RecipeParameter {
    pub name: String,
    pub kind: RecipeParameterKind,
    pub required: bool,
    pub default: Option<String>,
}

/// One immutable recipe revision.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct DevelopmentRecipe {
    pub id: String,
    pub version: u32,
    pub kind: DevelopmentRecipeKind,
    pub title: String,
    pub objective: String,
    pub criteria: Vec<String>,
    pub parameters: Vec<RecipeParameter>,
    pub required_skills: Vec<RecipeReference>,
    pub verification_packs: Vec<RecipeReference>,
    pub risk_level: RecipeRiskLevel,
    pub risk_reason: String,
    pub minimum_gate: RecipeGate,
}

/// Import/export contract for versioned recipes.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct DevelopmentRecipePack {
    pub schema_version: u8,
    pub recipes: Vec<DevelopmentRecipe>,
}

/// Exact installed references used during import and preview.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct RecipeReferenceCatalog {
    pub skills: BTreeSet<RecipeReference>,
    pub verification_packs: BTreeSet<RecipeReference>,
}

/// Trusted task context used for applicability and risk escalation.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct RecipeContext {
    pub kind: DevelopmentRecipeKind,
    pub database_change: bool,
    pub permission_change: bool,
    pub external_write: bool,
}

/// Immutable, side-effect-free recipe preview.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct RecipePreview {
    pub recipe_id: String,
    pub recipe_version: u32,
    pub kind: DevelopmentRecipeKind,
    pub title: String,
    pub objective: String,
    pub criteria: Vec<String>,
    pub resolved_inputs: BTreeMap<String, String>,
    pub risk_level: RecipeRiskLevel,
    pub risk_reason: String,
    pub required_gate: RecipeGate,
    pub preview_only: bool,
    pub preview_digest: String,
}

/// Task creation intent produced only from an exact confirmed preview.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ConfirmedRecipeTask {
    pub task_key: String,
    pub snapshot: RecipePreview,
}

/// Daily or interval schedule. Daily schedules use an IANA time zone.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum AutomationSchedule {
    Interval {
        seconds: u32,
    },
    Daily {
        time_zone: String,
        hour: u8,
        minute: u8,
    },
}

/// Durable automation rule. Editing creates a new revision.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct AutomationRule {
    pub id: String,
    pub revision: u32,
    pub enabled: bool,
    pub recipe_id: String,
    pub recipe_version: u32,
    pub inputs: BTreeMap<String, String>,
    pub context: RecipeContext,
    pub administrator_gate: RecipeGate,
    pub schedule: AutomationSchedule,
    pub ttl_seconds: u32,
    pub max_catch_up: u8,
}

/// Exact scheduled occurrence and resolved recipe snapshot.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct AutomationOccurrence {
    pub occurrence_id: String,
    pub rule_id: String,
    pub rule_revision: u32,
    pub scheduled_at: String,
    pub time_zone: String,
    pub expires_at: String,
    pub task: ConfirmedRecipeTask,
}

/// Independent status for one batch item.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum RecipeBatchItemStatus {
    Planned,
    Completed,
    Failed,
    Cancelled,
}

/// One independently cancellable and idempotent batch item.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct RecipeBatchItem {
    pub item_id: String,
    pub input_key: String,
    pub task: ConfirmedRecipeTask,
    pub status: RecipeBatchItemStatus,
}

/// Batch plan whose item outcomes never roll back sibling items.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct RecipeBatch {
    pub batch_id: String,
    pub items: BTreeMap<String, RecipeBatchItem>,
}

/// Statistics for one recipe revision and one task family only.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct RecipeVersionStatistics {
    pub recipe_id: String,
    pub recipe_version: u32,
    pub kind: DevelopmentRecipeKind,
    pub completed: u32,
    pub failed: u32,
    pub cancelled: u32,
    pub ranking_eligible: bool,
}

/// Stable recipe or automation validation failure.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum AutomationRecipeError {
    InvalidJson,
    UnsupportedVersion,
    InvalidRecipe,
    DuplicateRecipe,
    MissingReference,
    NotApplicable,
    InvalidInput,
    ConfirmationMismatch,
    InvalidSchedule,
    DuplicateBatchItem,
    InvalidBatchTransition,
}

impl fmt::Display for AutomationRecipeError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(match self {
            Self::InvalidJson => "recipe pack is not valid JSON",
            Self::UnsupportedVersion => "recipe pack schema version is unsupported",
            Self::InvalidRecipe => "recipe pack contains an invalid recipe",
            Self::DuplicateRecipe => "recipe pack contains a duplicate recipe revision",
            Self::MissingReference => "recipe references an unavailable exact version",
            Self::NotApplicable => "recipe does not apply to this task family",
            Self::InvalidInput => "recipe input is invalid",
            Self::ConfirmationMismatch => "recipe confirmation does not match the preview",
            Self::InvalidSchedule => "automation schedule is invalid",
            Self::DuplicateBatchItem => "batch contains a duplicate item",
            Self::InvalidBatchTransition => "batch item transition is invalid",
        })
    }
}

impl std::error::Error for AutomationRecipeError {}

impl DevelopmentRecipePack {
    /// Parses and validates an imported recipe pack.
    ///
    /// # Errors
    ///
    /// Rejects oversized JSON, unknown fields, malformed recipes and missing exact references.
    pub fn from_json(
        bytes: &[u8],
        catalog: &RecipeReferenceCatalog,
    ) -> Result<Self, AutomationRecipeError> {
        if bytes.len() > MAX_IMPORT_BYTES {
            return Err(AutomationRecipeError::InvalidJson);
        }
        let pack: Self =
            serde_json::from_slice(bytes).map_err(|_| AutomationRecipeError::InvalidJson)?;
        pack.validate(catalog)?;
        Ok(pack)
    }

    /// Returns the six first-party development recipes.
    #[must_use]
    pub fn built_in() -> Self {
        let verification = vec![RecipeReference {
            id: "verification.standard".to_owned(),
            version: 1,
        }];
        let make = |id: &str, kind, title: &str, risk_level, minimum_gate, risk_reason: &str| {
            DevelopmentRecipe {
                id: id.to_owned(),
                version: 1,
                kind,
                title: title.to_owned(),
                objective: "${goal}".to_owned(),
                criteria: vec!["Required machine verification passes".to_owned()],
                parameters: vec![RecipeParameter {
                    name: "goal".to_owned(),
                    kind: RecipeParameterKind::Text,
                    required: true,
                    default: None,
                }],
                required_skills: Vec::new(),
                verification_packs: verification.clone(),
                risk_level,
                risk_reason: risk_reason.to_owned(),
                minimum_gate,
            }
        };
        Self {
            schema_version: SCHEMA_VERSION,
            recipes: vec![
                make(
                    "bug-fix",
                    DevelopmentRecipeKind::BugFix,
                    "Bug fix",
                    RecipeRiskLevel::Medium,
                    RecipeGate::Review,
                    "changes existing behavior",
                ),
                make(
                    "api-development",
                    DevelopmentRecipeKind::ApiDevelopment,
                    "API development",
                    RecipeRiskLevel::Medium,
                    RecipeGate::Review,
                    "changes a public contract",
                ),
                make(
                    "page-development",
                    DevelopmentRecipeKind::PageDevelopment,
                    "Page development",
                    RecipeRiskLevel::Low,
                    RecipeGate::Standard,
                    "changes a user-facing surface",
                ),
                make(
                    "test-coverage",
                    DevelopmentRecipeKind::TestCoverage,
                    "Test coverage",
                    RecipeRiskLevel::Low,
                    RecipeGate::Standard,
                    "changes verification code",
                ),
                make(
                    "dependency-upgrade",
                    DevelopmentRecipeKind::DependencyUpgrade,
                    "Dependency upgrade",
                    RecipeRiskLevel::High,
                    RecipeGate::HumanApproval,
                    "changes third-party code",
                ),
                make(
                    "refactor",
                    DevelopmentRecipeKind::Refactor,
                    "Refactor",
                    RecipeRiskLevel::Medium,
                    RecipeGate::Review,
                    "changes implementation structure",
                ),
            ],
        }
    }

    /// Validates every recipe against exact installed references.
    ///
    /// # Errors
    ///
    /// Returns the first stable validation category.
    pub fn validate(&self, catalog: &RecipeReferenceCatalog) -> Result<(), AutomationRecipeError> {
        if self.schema_version != SCHEMA_VERSION {
            return Err(AutomationRecipeError::UnsupportedVersion);
        }
        if self.recipes.is_empty() || self.recipes.len() > 256 {
            return Err(AutomationRecipeError::InvalidRecipe);
        }
        let mut identities = BTreeSet::new();
        for recipe in &self.recipes {
            if !identities.insert((recipe.id.as_str(), recipe.version)) {
                return Err(AutomationRecipeError::DuplicateRecipe);
            }
            validate_recipe(recipe)?;
            if recipe
                .required_skills
                .iter()
                .any(|reference| !catalog.skills.contains(reference))
                || recipe
                    .verification_packs
                    .iter()
                    .any(|reference| !catalog.verification_packs.contains(reference))
            {
                return Err(AutomationRecipeError::MissingReference);
            }
        }
        Ok(())
    }

    /// Resolves an immutable preview without creating a task or executing a tool.
    ///
    /// # Errors
    ///
    /// Rejects missing references, non-applicable recipes and malformed inputs.
    pub fn preview(
        &self,
        recipe_id: &str,
        recipe_version: u32,
        context: RecipeContext,
        inputs: &BTreeMap<String, String>,
        administrator_gate: RecipeGate,
        catalog: &RecipeReferenceCatalog,
    ) -> Result<RecipePreview, AutomationRecipeError> {
        self.validate(catalog)?;
        let recipe = self
            .recipes
            .iter()
            .find(|recipe| recipe.id == recipe_id && recipe.version == recipe_version)
            .ok_or(AutomationRecipeError::InvalidRecipe)?;
        if recipe.kind != context.kind {
            return Err(AutomationRecipeError::NotApplicable);
        }
        let resolved_inputs = resolve_inputs(recipe, inputs)?;
        let (risk_level, risk_reason, gate) = effective_risk(recipe, context, administrator_gate);
        let mut preview = RecipePreview {
            recipe_id: recipe.id.clone(),
            recipe_version: recipe.version,
            kind: recipe.kind,
            title: recipe.title.clone(),
            objective: render(&recipe.objective, &resolved_inputs)?,
            criteria: recipe
                .criteria
                .iter()
                .map(|criterion| render(criterion, &resolved_inputs))
                .collect::<Result<_, _>>()?,
            resolved_inputs,
            risk_level,
            risk_reason,
            required_gate: gate,
            preview_only: true,
            preview_digest: String::new(),
        };
        preview.preview_digest = preview_digest(&preview)?;
        Ok(preview)
    }

    /// Converts an exact user-confirmed preview into one idempotent task intent.
    ///
    /// # Errors
    ///
    /// Rejects changed, stale or fabricated preview confirmation digests.
    pub fn confirm(
        &self,
        preview: &RecipePreview,
        confirmed_digest: &str,
    ) -> Result<ConfirmedRecipeTask, AutomationRecipeError> {
        if !preview.preview_only
            || preview.preview_digest != confirmed_digest
            || preview_digest(preview)? != confirmed_digest
            || !self.recipes.iter().any(|recipe| {
                recipe.id == preview.recipe_id && recipe.version == preview.recipe_version
            })
        {
            return Err(AutomationRecipeError::ConfirmationMismatch);
        }
        Ok(ConfirmedRecipeTask {
            task_key: digest_id("rtask", b"winwincode.recipe-task.v1", confirmed_digest),
            snapshot: preview.clone(),
        })
    }

    /// Resolves the first scheduled occurrence strictly after `after`.
    ///
    /// The occurrence identity excludes the rule revision. Persisting it through
    /// the existing idempotent Controller command path keeps an already-resolved
    /// occurrence pinned when a rule is edited.
    ///
    /// # Errors
    ///
    /// Rejects disabled or malformed rules, timestamps and recipe inputs.
    pub fn next_occurrence(
        &self,
        rule: &AutomationRule,
        after: &str,
        catalog: &RecipeReferenceCatalog,
    ) -> Result<AutomationOccurrence, AutomationRecipeError> {
        validate_rule(rule)?;
        let after =
            Timestamp::from_str(after).map_err(|_| AutomationRecipeError::InvalidSchedule)?;
        let (scheduled, time_zone) = next_schedule(&rule.schedule, after)?;
        let preview = self.preview(
            &rule.recipe_id,
            rule.recipe_version,
            rule.context,
            &rule.inputs,
            rule.administrator_gate,
            catalog,
        )?;
        let scheduled_at = scheduled.to_string();
        let occurrence_id = digest_id(
            "occ",
            b"winwincode.automation-occurrence.v1",
            &format!("{}\0{scheduled_at}", rule.id),
        );
        let mut task = self.confirm(&preview, &preview.preview_digest)?;
        task.task_key = digest_id(
            "rtask",
            b"winwincode.automation-recipe-occurrence-task.v1",
            &occurrence_id,
        );
        let expires_at = scheduled
            .checked_add(SignedDuration::from_secs(i64::from(rule.ttl_seconds)))
            .map_err(|_| AutomationRecipeError::InvalidSchedule)?
            .to_string();
        Ok(AutomationOccurrence {
            occurrence_id,
            rule_id: rule.id.clone(),
            rule_revision: rule.revision,
            scheduled_at,
            time_zone,
            expires_at,
            task,
        })
    }
}

impl RecipeBatch {
    /// Builds independent idempotent items from confirmed task snapshots.
    ///
    /// # Errors
    ///
    /// Rejects duplicate input keys.
    pub fn new(
        batch_id: impl Into<String>,
        tasks: impl IntoIterator<Item = (String, ConfirmedRecipeTask)>,
    ) -> Result<Self, AutomationRecipeError> {
        let batch_id = batch_id.into();
        if !portable(&batch_id) {
            return Err(AutomationRecipeError::InvalidInput);
        }
        let mut items = BTreeMap::new();
        for (input_key, task) in tasks {
            if !portable(&input_key) {
                return Err(AutomationRecipeError::InvalidInput);
            }
            let item_id = digest_id(
                "bti",
                b"winwincode.recipe-batch-item.v1",
                &format!("{batch_id}\0{input_key}"),
            );
            if items
                .insert(
                    item_id.clone(),
                    RecipeBatchItem {
                        item_id,
                        input_key,
                        task,
                        status: RecipeBatchItemStatus::Planned,
                    },
                )
                .is_some()
            {
                return Err(AutomationRecipeError::DuplicateBatchItem);
            }
        }
        Ok(Self { batch_id, items })
    }

    /// Cancels one unstarted item without changing completed siblings.
    ///
    /// # Errors
    ///
    /// Rejects unknown or already settled items.
    pub fn cancel(&mut self, item_id: &str) -> Result<(), AutomationRecipeError> {
        self.settle(item_id, RecipeBatchItemStatus::Cancelled)
    }

    /// Records one independent item outcome.
    ///
    /// # Errors
    ///
    /// Rejects unknown items, `Planned`, or a second settlement.
    pub fn settle(
        &mut self,
        item_id: &str,
        status: RecipeBatchItemStatus,
    ) -> Result<(), AutomationRecipeError> {
        if status == RecipeBatchItemStatus::Planned {
            return Err(AutomationRecipeError::InvalidBatchTransition);
        }
        let item = self
            .items
            .get_mut(item_id)
            .ok_or(AutomationRecipeError::InvalidBatchTransition)?;
        if item.status != RecipeBatchItemStatus::Planned {
            return Err(AutomationRecipeError::InvalidBatchTransition);
        }
        item.status = status;
        Ok(())
    }

    /// Groups counts by exact recipe revision and task family.
    #[must_use]
    pub fn statistics(&self) -> Vec<RecipeVersionStatistics> {
        let mut groups = BTreeMap::<(String, u32, DevelopmentRecipeKind), (u32, u32, u32)>::new();
        for item in self.items.values() {
            let key = (
                item.task.snapshot.recipe_id.clone(),
                item.task.snapshot.recipe_version,
                item.task.snapshot.kind,
            );
            let counts = groups.entry(key).or_default();
            match item.status {
                RecipeBatchItemStatus::Completed => counts.0 += 1,
                RecipeBatchItemStatus::Failed => counts.1 += 1,
                RecipeBatchItemStatus::Cancelled => counts.2 += 1,
                RecipeBatchItemStatus::Planned => {}
            }
        }
        groups
            .into_iter()
            .map(
                |((recipe_id, recipe_version, kind), (completed, failed, cancelled))| {
                    let total = completed + failed + cancelled;
                    RecipeVersionStatistics {
                        recipe_id,
                        recipe_version,
                        kind,
                        completed,
                        failed,
                        cancelled,
                        ranking_eligible: total >= MIN_RANKING_SAMPLE,
                    }
                },
            )
            .collect()
    }
}

fn validate_recipe(recipe: &DevelopmentRecipe) -> Result<(), AutomationRecipeError> {
    if !portable_id(&recipe.id)
        || recipe.version == 0
        || !portable(&recipe.title)
        || !portable(&recipe.objective)
        || recipe.criteria.is_empty()
        || recipe.criteria.len() > 32
        || recipe.criteria.iter().any(|value| !portable(value))
        || !portable(&recipe.risk_reason)
        || recipe.parameters.len() > 64
        || !unique(&recipe.required_skills)
        || !unique(&recipe.verification_packs)
        || recipe
            .required_skills
            .iter()
            .chain(&recipe.verification_packs)
            .any(|reference| !portable_id(&reference.id) || reference.version == 0)
    {
        return Err(AutomationRecipeError::InvalidRecipe);
    }
    let mut names = BTreeSet::new();
    for parameter in &recipe.parameters {
        if !portable_id(&parameter.name)
            || !names.insert(parameter.name.as_str())
            || (parameter.kind == RecipeParameterKind::CredentialReference
                && parameter.default.is_some())
            || (secret_name(&parameter.name)
                && parameter.kind != RecipeParameterKind::CredentialReference)
            || parameter.default.as_deref().is_some_and(unsafe_default)
        {
            return Err(AutomationRecipeError::InvalidRecipe);
        }
    }
    Ok(())
}

fn resolve_inputs(
    recipe: &DevelopmentRecipe,
    inputs: &BTreeMap<String, String>,
) -> Result<BTreeMap<String, String>, AutomationRecipeError> {
    if inputs.keys().any(|name| {
        !recipe
            .parameters
            .iter()
            .any(|parameter| &parameter.name == name)
    }) {
        return Err(AutomationRecipeError::InvalidInput);
    }
    let mut resolved = BTreeMap::new();
    for parameter in &recipe.parameters {
        let value = inputs.get(&parameter.name).or(parameter.default.as_ref());
        let Some(value) = value else {
            if parameter.required {
                return Err(AutomationRecipeError::InvalidInput);
            }
            continue;
        };
        if !portable(value)
            || (parameter.kind == RecipeParameterKind::CredentialReference
                && !canonical_credential_reference(value))
        {
            return Err(AutomationRecipeError::InvalidInput);
        }
        resolved.insert(parameter.name.clone(), value.clone());
    }
    Ok(resolved)
}

fn effective_risk(
    recipe: &DevelopmentRecipe,
    context: RecipeContext,
    administrator_gate: RecipeGate,
) -> (RecipeRiskLevel, String, RecipeGate) {
    let elevated = context.database_change || context.permission_change || context.external_write;
    let risk = if elevated {
        RecipeRiskLevel::High
    } else {
        recipe.risk_level
    };
    let reason = if elevated {
        format!(
            "{}; database, permission, or external write",
            recipe.risk_reason
        )
    } else {
        recipe.risk_reason.clone()
    };
    let recipe_gate = if elevated {
        RecipeGate::HumanApproval
    } else {
        recipe.minimum_gate
    };
    (risk, reason, recipe_gate.max(administrator_gate))
}

fn render(
    template: &str,
    inputs: &BTreeMap<String, String>,
) -> Result<String, AutomationRecipeError> {
    let rendered = inputs
        .iter()
        .fold(template.to_owned(), |rendered, (name, value)| {
            rendered.replace(&format!("${{{name}}}"), value)
        });
    if rendered.contains("${") || !portable(&rendered) {
        return Err(AutomationRecipeError::InvalidInput);
    }
    Ok(rendered)
}

fn preview_digest(preview: &RecipePreview) -> Result<String, AutomationRecipeError> {
    let mut canonical = preview.clone();
    canonical.preview_digest.clear();
    let bytes = serde_json::to_vec(&canonical).map_err(|_| AutomationRecipeError::InvalidInput)?;
    Ok(format!("sha256:{:x}", Sha256::digest(bytes)))
}

fn validate_rule(rule: &AutomationRule) -> Result<(), AutomationRecipeError> {
    if !rule.enabled
        || !portable_id(&rule.id)
        || rule.revision == 0
        || rule.recipe_version == 0
        || rule.ttl_seconds == 0
        || rule.max_catch_up == 0
        || rule.max_catch_up > 32
    {
        return Err(AutomationRecipeError::InvalidSchedule);
    }
    Ok(())
}

fn next_schedule(
    schedule: &AutomationSchedule,
    after: Timestamp,
) -> Result<(Timestamp, String), AutomationRecipeError> {
    match schedule {
        AutomationSchedule::Interval { seconds } if *seconds > 0 => after
            .checked_add(SignedDuration::from_secs(i64::from(*seconds)))
            .map(|timestamp| (timestamp, "UTC".to_owned()))
            .map_err(|_| AutomationRecipeError::InvalidSchedule),
        AutomationSchedule::Daily {
            time_zone,
            hour,
            minute,
        } if *hour < 24 && *minute < 60 => {
            let zone =
                TimeZone::get(time_zone).map_err(|_| AutomationRecipeError::InvalidSchedule)?;
            let local = zone.to_datetime(after);
            let candidate: DateTime = local
                .with()
                .hour(i8::try_from(*hour).map_err(|_| AutomationRecipeError::InvalidSchedule)?)
                .minute(i8::try_from(*minute).map_err(|_| AutomationRecipeError::InvalidSchedule)?)
                .second(0)
                .nanosecond(0)
                .build()
                .map_err(|_| AutomationRecipeError::InvalidSchedule)?;
            let mut scheduled = zone
                .to_ambiguous_zoned(candidate)
                .later()
                .map_err(|_| AutomationRecipeError::InvalidSchedule)?;
            if scheduled.timestamp() <= after {
                scheduled = zone
                    .to_ambiguous_zoned(
                        candidate
                            .tomorrow()
                            .map_err(|_| AutomationRecipeError::InvalidSchedule)?,
                    )
                    .later()
                    .map_err(|_| AutomationRecipeError::InvalidSchedule)?;
            }
            Ok((scheduled.timestamp(), time_zone.clone()))
        }
        AutomationSchedule::Interval { .. } | AutomationSchedule::Daily { .. } => {
            Err(AutomationRecipeError::InvalidSchedule)
        }
    }
}

fn digest_id(prefix: &str, domain: &[u8], value: &str) -> String {
    let mut digest = Sha256::new();
    digest.update(domain);
    digest.update([0]);
    digest.update(value.as_bytes());
    let hex = format!("{:x}", digest.finalize());
    format!("{prefix}_{}", &hex[..26].to_ascii_uppercase())
}

fn unique<T: Ord>(values: &[T]) -> bool {
    values.iter().collect::<BTreeSet<_>>().len() == values.len()
}

fn portable(value: &str) -> bool {
    !value.trim().is_empty() && value.len() <= 2_000 && !value.chars().any(char::is_control)
}

fn portable_id(value: &str) -> bool {
    portable(value)
        && value.len() <= 128
        && value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'_' | b'-'))
}

fn secret_name(value: &str) -> bool {
    let value = value.to_ascii_lowercase();
    [
        "secret",
        "password",
        "token",
        "api_key",
        "apikey",
        "private_key",
    ]
    .iter()
    .any(|marker| value.contains(marker))
}

fn unsafe_default(value: &str) -> bool {
    !portable(value)
        || value.contains('@')
        || value.starts_with('/')
        || value.contains("://")
        || ["bearer ", "password=", "secret=", "token=", "api_key="]
            .iter()
            .any(|marker| value.to_ascii_lowercase().contains(marker))
}

fn canonical_credential_reference(value: &str) -> bool {
    value.strip_prefix("crd_").is_some_and(|suffix| {
        suffix.len() == 26 && suffix.bytes().all(|byte| byte.is_ascii_alphanumeric())
    })
}
