// SPDX-License-Identifier: Apache-2.0

//! Repository-local deterministic rules for machine-derived post-action work.

use std::collections::{BTreeMap, BTreeSet};
use std::fmt;

use serde::{Deserialize, Serialize};

/// Canonical path read from the frozen repository snapshot.
pub const REPOSITORY_RULE_PACK_PATH: &str = ".winwincode/rules.json";
/// Only accepted rule-pack schema.
pub const REPOSITORY_RULE_PACK_SCHEMA_VERSION: u8 = 1;

const MAX_RULES: usize = 256;
const MAX_TEXT: usize = 500;

/// Runtime event kinds understood by the repository rule engine.
#[derive(Clone, Copy, Debug, Deserialize, Eq, Ord, PartialEq, PartialOrd, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum RepositoryRuleEvent {
    FileChanged,
    CommandStarted,
    CommandFinished,
    TestFinished,
    DependencyChanged,
    GitStateChanged,
    CandidateReady,
}

/// Trusted executor result used by a rule condition.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum PostActionOutcome {
    Succeeded,
    Failed,
}

/// Closed machine action emitted by repository rules.
#[derive(Clone, Copy, Debug, Deserialize, Eq, Ord, PartialEq, PartialOrd, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum PostActionHook {
    RequireVerification,
    CreateMachineBlocker,
}

/// Exact normalized fact presented to the pure evaluator.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RepositoryRuleFact<'fact> {
    pub event: RepositoryRuleEvent,
    pub language: Option<&'fact str>,
    pub path: Option<&'fact str>,
    pub outcome: Option<PostActionOutcome>,
}

/// One versioned repository rule.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct RepositoryRule {
    pub id: String,
    pub version: u32,
    pub event: RepositoryRuleEvent,
    #[serde(default)]
    pub languages: Vec<String>,
    #[serde(default)]
    pub file_patterns: Vec<String>,
    pub outcome: Option<PostActionOutcome>,
    pub actions: Vec<PostActionHook>,
    pub priority: u16,
}

/// Current repository-local rule-pack contract.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct RepositoryRulePack {
    pub schema_version: u8,
    pub rules: Vec<RepositoryRule>,
}

/// Why one rule matched during a dry run.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RepositoryRuleMatch {
    pub rule_id: String,
    pub rule_version: u32,
    pub priority: u16,
    pub language: Option<String>,
    pub file_pattern: Option<String>,
    pub actions: Vec<PostActionHook>,
}

/// Deterministic explanation and action set. Constructing it performs no action.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RepositoryRuleEvaluation {
    pub matched_rules: Vec<RepositoryRuleMatch>,
    pub actions: Vec<PostActionHook>,
}

/// Closed lint/parse failure for a repository rule pack.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum RepositoryRulePackError {
    InvalidJson,
    UnsupportedVersion,
    TooManyRules,
    InvalidRule,
    DuplicateRule,
}

impl fmt::Display for RepositoryRulePackError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(match self {
            Self::InvalidJson => "repository rule pack is not valid JSON",
            Self::UnsupportedVersion => "repository rule pack schema version is unsupported",
            Self::TooManyRules => "repository rule pack exceeds 256 rules",
            Self::InvalidRule => "repository rule pack contains an invalid rule",
            Self::DuplicateRule => "repository rule pack contains a duplicate rule",
        })
    }
}

impl std::error::Error for RepositoryRulePackError {}

impl RepositoryRulePack {
    /// Parses and lints the exact bytes read from [`REPOSITORY_RULE_PACK_PATH`].
    ///
    /// # Errors
    ///
    /// Rejects unknown fields, another schema version, or ambiguous rules.
    pub fn from_json(bytes: &[u8]) -> Result<Self, RepositoryRulePackError> {
        let pack: Self =
            serde_json::from_slice(bytes).map_err(|_| RepositoryRulePackError::InvalidJson)?;
        pack.lint()?;
        Ok(pack)
    }

    /// Built-in project behavior used when the repository has no local pack.
    #[must_use]
    pub fn project_defaults() -> Self {
        Self {
            schema_version: REPOSITORY_RULE_PACK_SCHEMA_VERSION,
            rules: vec![
                RepositoryRule {
                    id: "default.command-failure-blocker".into(),
                    version: 1,
                    event: RepositoryRuleEvent::CommandFinished,
                    languages: Vec::new(),
                    file_patterns: Vec::new(),
                    outcome: Some(PostActionOutcome::Failed),
                    actions: vec![PostActionHook::CreateMachineBlocker],
                    priority: 0,
                },
                RepositoryRule {
                    id: "default.file-change-verification".into(),
                    version: 1,
                    event: RepositoryRuleEvent::FileChanged,
                    languages: Vec::new(),
                    file_patterns: Vec::new(),
                    outcome: Some(PostActionOutcome::Succeeded),
                    actions: vec![PostActionHook::RequireVerification],
                    priority: 0,
                },
            ],
        }
    }

    /// Adds repository rules to the project defaults. An identical ID replaces
    /// its default, which is the single conflict policy.
    ///
    /// # Errors
    ///
    /// Rejects either invalid input before producing the effective pack.
    pub fn with_project_defaults(self) -> Result<Self, RepositoryRulePackError> {
        self.lint()?;
        let mut rules = RepositoryRulePack::project_defaults()
            .rules
            .into_iter()
            .map(|rule| (rule.id.clone(), rule))
            .collect::<BTreeMap<_, _>>();
        for rule in self.rules {
            rules.insert(rule.id.clone(), rule);
        }
        let pack = Self {
            schema_version: REPOSITORY_RULE_PACK_SCHEMA_VERSION,
            rules: rules.into_values().collect(),
        };
        pack.lint()?;
        Ok(pack)
    }

    /// Validates rule identities, conditions, actions, and bounds.
    ///
    /// # Errors
    ///
    /// Returns the first stable lint category.
    pub fn lint(&self) -> Result<(), RepositoryRulePackError> {
        if self.schema_version != REPOSITORY_RULE_PACK_SCHEMA_VERSION {
            return Err(RepositoryRulePackError::UnsupportedVersion);
        }
        if self.rules.len() > MAX_RULES {
            return Err(RepositoryRulePackError::TooManyRules);
        }
        let mut ids = BTreeSet::new();
        for rule in &self.rules {
            if !portable_rule_id(&rule.id)
                || rule.version == 0
                || rule.priority > 10_000
                || rule.actions.is_empty()
                || !unique(&rule.languages)
                || !unique(&rule.file_patterns)
                || !unique(&rule.actions)
                || !rule.languages.iter().all(|value| portable_language(value))
                || !rule
                    .file_patterns
                    .iter()
                    .all(|value| portable_pattern(value))
                || ((!rule.languages.is_empty() || !rule.file_patterns.is_empty())
                    && rule.event != RepositoryRuleEvent::FileChanged)
                || (rule.outcome.is_some()
                    && !matches!(
                        rule.event,
                        RepositoryRuleEvent::FileChanged
                            | RepositoryRuleEvent::CommandFinished
                            | RepositoryRuleEvent::TestFinished
                    ))
            {
                return Err(RepositoryRulePackError::InvalidRule);
            }
            if !ids.insert(&rule.id) {
                return Err(RepositoryRulePackError::DuplicateRule);
            }
        }
        Ok(())
    }

    /// Evaluates and explains one fact without executing the returned actions.
    ///
    /// # Errors
    ///
    /// Rejects an invalid pack or malformed input fact.
    pub fn dry_run(
        &self,
        fact: &RepositoryRuleFact<'_>,
    ) -> Result<RepositoryRuleEvaluation, RepositoryRulePackError> {
        self.lint()?;
        if fact.language.is_some_and(|value| !portable_language(value))
            || fact.path.is_some_and(|value| !portable_path(value))
            || (fact.language.is_some() && fact.path.is_none())
            || ((fact.language.is_some() || fact.path.is_some())
                && fact.event != RepositoryRuleEvent::FileChanged)
            || (fact.outcome.is_some()
                && !matches!(
                    fact.event,
                    RepositoryRuleEvent::FileChanged
                        | RepositoryRuleEvent::CommandFinished
                        | RepositoryRuleEvent::TestFinished
                ))
            || (fact.outcome.is_none()
                && matches!(
                    fact.event,
                    RepositoryRuleEvent::FileChanged
                        | RepositoryRuleEvent::CommandFinished
                        | RepositoryRuleEvent::TestFinished
                ))
        {
            return Err(RepositoryRulePackError::InvalidRule);
        }
        let mut matched_rules = self
            .rules
            .iter()
            .filter_map(|rule| match_rule(rule, fact))
            .collect::<Vec<_>>();
        matched_rules.sort_by(|left, right| {
            right
                .priority
                .cmp(&left.priority)
                .then_with(|| left.rule_id.cmp(&right.rule_id))
        });
        let actions = matched_rules
            .iter()
            .flat_map(|matched| matched.actions.iter().copied())
            .collect::<BTreeSet<_>>()
            .into_iter()
            .collect();
        Ok(RepositoryRuleEvaluation {
            matched_rules,
            actions,
        })
    }
}

fn match_rule(rule: &RepositoryRule, fact: &RepositoryRuleFact<'_>) -> Option<RepositoryRuleMatch> {
    if rule.event != fact.event
        || rule
            .outcome
            .is_some_and(|value| Some(value) != fact.outcome)
    {
        return None;
    }
    let language = if rule.languages.is_empty() {
        None
    } else {
        let value = fact.language?;
        Some(
            rule.languages
                .iter()
                .find(|expected| expected.eq_ignore_ascii_case(value))
                .cloned()?,
        )
    };
    let file_pattern = if rule.file_patterns.is_empty() {
        None
    } else {
        let path = fact.path?;
        Some(
            rule.file_patterns
                .iter()
                .find(|pattern| wildcard_matches(pattern, path))
                .cloned()?,
        )
    };
    Some(RepositoryRuleMatch {
        rule_id: rule.id.clone(),
        rule_version: rule.version,
        priority: rule.priority,
        language,
        file_pattern,
        actions: rule.actions.clone(),
    })
}

fn wildcard_matches(pattern: &str, value: &str) -> bool {
    let pattern = pattern.as_bytes();
    let value = value.as_bytes();
    let (mut pattern_index, mut value_index) = (0, 0);
    let (mut star_index, mut star_value_index) = (None, 0);
    while value_index < value.len() {
        if pattern.get(pattern_index) == value.get(value_index) {
            pattern_index += 1;
            value_index += 1;
        } else if pattern.get(pattern_index) == Some(&b'*') {
            star_index = Some(pattern_index);
            pattern_index += 1;
            star_value_index = value_index;
        } else if let Some(star) = star_index {
            star_value_index += 1;
            value_index = star_value_index;
            pattern_index = star + 1;
        } else {
            return false;
        }
    }
    while pattern.get(pattern_index) == Some(&b'*') {
        pattern_index += 1;
    }
    pattern_index == pattern.len()
}

fn unique<T: Ord>(values: &[T]) -> bool {
    values.iter().collect::<BTreeSet<_>>().len() == values.len()
}

fn portable_rule_id(value: &str) -> bool {
    portable(value)
        && value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'_' | b'-'))
}

fn portable_language(value: &str) -> bool {
    portable(value)
        && value.bytes().all(|byte| {
            byte.is_ascii_lowercase() || byte.is_ascii_digit() || matches!(byte, b'+' | b'#' | b'-')
        })
}

fn portable_pattern(value: &str) -> bool {
    portable_path(value) && !value.starts_with('/') && !value.split('/').any(|part| part == "..")
}

fn portable_path(value: &str) -> bool {
    portable(value) && !value.contains('\\')
}

fn portable(value: &str) -> bool {
    !value.is_empty() && value.len() <= MAX_TEXT && !value.chars().any(char::is_control)
}

/// Returns the canonical language label derived only from a repository path.
#[must_use]
pub fn language_for_path(path: &str) -> Option<&'static str> {
    let extension = path.rsplit_once('.')?.1.to_ascii_lowercase();
    Some(match extension.as_str() {
        "c" | "h" => "c",
        "cc" | "cpp" | "cxx" | "hpp" => "cpp",
        "cs" => "csharp",
        "go" => "go",
        "java" => "java",
        "js" | "mjs" | "cjs" => "javascript",
        "jsx" => "jsx",
        "kt" | "kts" => "kotlin",
        "php" => "php",
        "py" => "python",
        "rb" => "ruby",
        "rs" => "rust",
        "swift" => "swift",
        "ts" | "mts" | "cts" => "typescript",
        "tsx" => "tsx",
        _ => return None,
    })
}
