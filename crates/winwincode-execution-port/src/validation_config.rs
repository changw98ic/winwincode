// SPDX-License-Identifier: Apache-2.0

//! Canonical validation configuration parsing and profile selection.

use std::{collections::BTreeMap, fmt, path::Component};

use sha2::{Digest as _, Sha256};
use winwincode_domain::{Sha256Digest, WorkspaceRevision};

use crate::generated::{
    DiagnosticParserVersion, NormalizerReceipt, NormalizerReceiptStatus, ValidationCommandLanguage,
    ValidationCommandPhase, ValidationCommandSpec, ValidationConfiguration, ValidationProfileName,
    ValidationProfileSelection, ValidationProfileSelectionReasonCode, ValidationReceipt,
    ValidationReceiptStatus, ValidationSelectionSource,
};

/// Repository-relative location of the sole validation configuration.
pub const VALIDATION_CONFIGURATION_PATH: &str = ".winwincode/validation.toml";
/// Recommended command timeout that repository configurations may state explicitly.
pub const DEFAULT_VALIDATION_TIMEOUT_MILLIS: i64 = 300_000;
/// Recommended combined stdout/stderr limit that configurations may state explicitly.
pub const DEFAULT_VALIDATION_OUTPUT_LIMIT_BYTES: i64 = 1_048_576;
/// Parser bound before TOML decoding.
pub const MAX_VALIDATION_CONFIGURATION_BYTES: usize = 262_144;
/// Aggregate argv bound for one command.
pub const MAX_VALIDATION_ARGV_BYTES: usize = 65_536;

/// Stable validation-configuration failure classes.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ValidationConfigurationErrorCode {
    InvalidToml,
    InvalidConfiguration,
    UnknownProfile,
    RevisionMismatch,
}

/// Bounded error that does not echo configuration or command contents.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ValidationConfigurationError {
    code: ValidationConfigurationErrorCode,
    message: &'static str,
}

impl ValidationConfigurationError {
    const fn new(code: ValidationConfigurationErrorCode, message: &'static str) -> Self {
        Self { code, message }
    }

    /// Returns the stable machine-readable failure class.
    #[must_use]
    pub const fn code(self) -> ValidationConfigurationErrorCode {
        self.code
    }
}

impl fmt::Display for ValidationConfigurationError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.message)
    }
}

impl std::error::Error for ValidationConfigurationError {}

/// Strictly parsed configuration and its byte-exact identity.
#[derive(Clone, Debug, PartialEq)]
pub struct ParsedValidationConfiguration {
    configuration: ValidationConfiguration,
    digest: Sha256Digest,
}

impl ParsedValidationConfiguration {
    /// Returns the canonical generated configuration.
    #[must_use]
    pub const fn configuration(&self) -> &ValidationConfiguration {
        &self.configuration
    }

    /// Returns the digest of the exact TOML bytes that were parsed.
    #[must_use]
    pub const fn digest(&self) -> &Sha256Digest {
        &self.digest
    }
}

/// Parses the one supported TOML shape and validates cross-reference rules.
///
/// # Errors
///
/// Rejects oversized, malformed, unknown-field, duplicate, unsafe-path, shell,
/// incomplete-language, or dangling-command configurations.
pub fn parse_validation_configuration(
    input: &str,
) -> Result<ParsedValidationConfiguration, ValidationConfigurationError> {
    if input.is_empty() || input.len() > MAX_VALIDATION_CONFIGURATION_BYTES || input.contains('\0')
    {
        return Err(invalid_configuration());
    }
    // Decode through TOML's own value model before applying the generated JSON
    // contract. This keeps TOML syntax handling separate while preserving the
    // generated DTO's unknown-field and exact-oneOf validation.
    let toml_value: toml::Value = toml::from_str(input).map_err(|_| invalid_toml())?;
    let json_value = serde_json::to_value(toml_value).map_err(|_| invalid_toml())?;
    let configuration: ValidationConfiguration =
        serde_json::from_value(json_value).map_err(|_| invalid_configuration())?;
    validate_configuration(&configuration)?;
    Ok(ParsedValidationConfiguration {
        configuration,
        digest: digest_framed(
            b"winwincode.validation.configuration.v1",
            [input.as_bytes()],
        ),
    })
}

/// Selects an executable profile from an explicit parsed configuration.
///
/// # Errors
///
/// Rejects an unknown profile or invalid changed path set.
pub fn select_configured_profile(
    parsed: &ParsedValidationConfiguration,
    requested: &str,
    changed_paths: &[String],
) -> Result<ValidationProfileSelection, ValidationConfigurationError> {
    let requested = parse_profile_name(requested).ok_or_else(unknown_profile)?;
    let profile = parsed
        .configuration
        .profiles
        .iter()
        .find(|profile| profile.name == requested)
        .ok_or_else(unknown_profile)?;
    Ok(ValidationProfileSelection {
        changed_paths_digest: digest_changed_paths(changed_paths)?,
        command_ids: profile.command_ids.clone(),
        configuration_digest: Some(parsed.digest.clone()),
        executable: true,
        profile: requested,
        reason_code: ValidationProfileSelectionReasonCode::ExplicitProfile,
        source: ValidationSelectionSource::ExplicitConfiguration,
    })
}

/// Produces a non-executable suggestion when no explicit configuration exists.
///
/// The returned command list is always empty, so inference cannot authorize a
/// process invocation.
///
/// # Errors
///
/// Rejects a duplicate or non-portable changed path.
pub fn suggest_validation_profile(
    changed_paths: &[String],
) -> Result<ValidationProfileSelection, ValidationConfigurationError> {
    let changed_paths_digest = digest_changed_paths(changed_paths)?;
    let (profile, reason_code) = suggested_profile(changed_paths);
    Ok(ValidationProfileSelection {
        changed_paths_digest,
        command_ids: Vec::new(),
        configuration_digest: None,
        executable: false,
        profile,
        reason_code,
        source: ValidationSelectionSource::AutomaticSuggestion,
    })
}

/// Resolves one profile with explicit configuration taking unconditional priority.
///
/// A missing configuration produces only a non-executable suggestion. An
/// invalid present configuration must fail during parsing and is never treated
/// as if the file were absent.
///
/// # Errors
///
/// Rejects an unknown explicit profile or invalid changed paths.
pub fn resolve_validation_profile(
    configuration: Option<&ParsedValidationConfiguration>,
    requested: &str,
    changed_paths: &[String],
) -> Result<ValidationProfileSelection, ValidationConfigurationError> {
    match configuration {
        Some(configuration) => select_configured_profile(configuration, requested, changed_paths),
        None => suggest_validation_profile(changed_paths),
    }
}

/// Verifies that a normalizer receipt binds the exact input and proven result tree.
///
/// # Errors
///
/// Rejects a revision mismatch or a status/result combination that claims more
/// exactness than was proven.
pub fn validate_normalizer_receipt_binding(
    receipt: &NormalizerReceipt,
    expected_base: &WorkspaceRevision,
    expected_result: Option<&WorkspaceRevision>,
) -> Result<(), ValidationConfigurationError> {
    if receipt.base_revision != *expected_base
        || receipt.result_revision.as_ref() != expected_result
        || match receipt.status {
            NormalizerReceiptStatus::Normalized => {
                expected_result.is_none_or(|result| result == expected_base)
                    || receipt.changed_file_digests.is_empty()
            }
            NormalizerReceiptStatus::Unchanged => {
                expected_result != Some(expected_base) || !receipt.changed_file_digests.is_empty()
            }
            NormalizerReceiptStatus::Rejected
            | NormalizerReceiptStatus::InfrastructureError
            | NormalizerReceiptStatus::Cancelled => {
                expected_result.is_some() || !receipt.changed_file_digests.is_empty()
            }
        }
    {
        return Err(revision_mismatch());
    }
    Ok(())
}

/// Verifies that validation ran against one exact result tree.
///
/// # Errors
///
/// Rejects non-canonical profiles, revision drift, or impossible status/check
/// combinations.
pub fn validate_validation_receipt_binding(
    receipt: &ValidationReceipt,
    expected_base: &WorkspaceRevision,
    expected_result: Option<&WorkspaceRevision>,
) -> Result<(), ValidationConfigurationError> {
    let has_exact_result = expected_result.is_some();
    let invalid_status_binding = match receipt.status {
        ValidationReceiptStatus::NotRun => {
            has_exact_result || !receipt.checks.is_empty() || receipt.duration_millis != 0
        }
        ValidationReceiptStatus::Passed | ValidationReceiptStatus::Failed => {
            !has_exact_result || receipt.checks.is_empty()
        }
        ValidationReceiptStatus::InfrastructureError | ValidationReceiptStatus::Cancelled => {
            !has_exact_result
        }
    };
    if receipt.base_revision != *expected_base
        || receipt.result_revision.as_ref() != expected_result
        || invalid_status_binding
    {
        return Err(revision_mismatch());
    }
    Ok(())
}

fn validate_configuration(
    configuration: &ValidationConfiguration,
) -> Result<(), ValidationConfigurationError> {
    let commands = validate_commands(configuration)?;
    validate_profiles(configuration, &commands)
}

fn validate_commands(
    configuration: &ValidationConfiguration,
) -> Result<BTreeMap<&str, &ValidationCommandSpec>, ValidationConfigurationError> {
    let mut commands = BTreeMap::new();
    let mut languages = [false; 3];
    for command in &configuration.commands {
        if commands.insert(command.id.as_str(), command).is_some()
            || !bounded_identifier(&command.id)
            || command.argv.is_empty()
            || command.argv.len() > 256
            || command
                .argv
                .iter()
                .any(|argument| argument.is_empty() || argument.len() > 4096)
            || command.argv.iter().map(String::len).sum::<usize>() > MAX_VALIDATION_ARGV_BYTES
            || !portable_working_directory(&command.working_directory)
            || is_shell_program(&command.argv[0])
            || command.network
            || !(1..=86_400_000).contains(&command.timeout_millis)
            || !(1..=16_777_216).contains(&command.output_limit_bytes)
            || command.environment.len() > 5
            || command.allowed_companion_paths.len() > 20
            || command
                .allowed_companion_paths
                .iter()
                .any(|path| !portable_relative_path(path))
            || {
                let mut paths = command.allowed_companion_paths.clone();
                paths.sort();
                paths.windows(2).any(|pair| pair[0] == pair[1])
            }
            || (matches!(command.phase, ValidationCommandPhase::Validation)
                && !command.allowed_companion_paths.is_empty())
            || (!matches!(command.phase, ValidationCommandPhase::Validation)
                && command.diagnostic_parser_version.is_some())
            || !parser_matches_language(command)
        {
            return Err(invalid_configuration());
        }
        let mut environment_names = Vec::new();
        for variable in &command.environment {
            if variable.value.len() > 4096 {
                return Err(invalid_configuration());
            }
            let name =
                serde_json::to_string(&variable.name).map_err(|_| invalid_configuration())?;
            if environment_names.contains(&name) {
                return Err(invalid_configuration());
            }
            environment_names.push(name);
        }
        match command.language {
            ValidationCommandLanguage::Typescript => languages[0] = true,
            ValidationCommandLanguage::Rust => languages[1] = true,
            ValidationCommandLanguage::Python => languages[2] = true,
            ValidationCommandLanguage::Go => {}
        }
    }
    if languages != [true, true, true] {
        return Err(invalid_configuration());
    }
    Ok(commands)
}

fn parser_matches_language(command: &ValidationCommandSpec) -> bool {
    matches!(
        (&command.language, &command.diagnostic_parser_version),
        (_, None | Some(DiagnosticParserVersion::JunitXmlV1))
            | (
                ValidationCommandLanguage::Typescript,
                Some(DiagnosticParserVersion::TypescriptV1 | DiagnosticParserVersion::EslintJsonV1),
            )
            | (
                ValidationCommandLanguage::Rust,
                Some(DiagnosticParserVersion::CargoJsonV1)
            )
            | (
                ValidationCommandLanguage::Python,
                Some(DiagnosticParserVersion::PytestJsonV1)
            )
            | (
                ValidationCommandLanguage::Go,
                Some(DiagnosticParserVersion::GoTestJsonV1)
            )
    )
}

fn validate_profiles(
    configuration: &ValidationConfiguration,
    commands: &BTreeMap<&str, &ValidationCommandSpec>,
) -> Result<(), ValidationConfigurationError> {
    let mut profile_names = Vec::new();
    let mut referenced_commands = std::collections::BTreeSet::new();
    for profile in &configuration.profiles {
        let name = profile_name_text(&profile.name);
        if profile_names.contains(&name)
            || profile.command_ids.is_empty()
            || profile.command_ids.len() > 64
            || profile.command_ids.iter().any(|id| !bounded_identifier(id))
            || {
                let mut command_ids = profile.command_ids.clone();
                command_ids.sort();
                command_ids.windows(2).any(|pair| pair[0] == pair[1])
            }
            || profile
                .command_ids
                .iter()
                .any(|command_id| !commands.contains_key(command_id.as_str()))
        {
            return Err(invalid_configuration());
        }
        let mut validation_seen = false;
        let mut companion_paths = std::collections::BTreeSet::new();
        for command_id in &profile.command_ids {
            referenced_commands.insert(command_id.as_str());
            let command = commands
                .get(command_id.as_str())
                .ok_or_else(invalid_configuration)?;
            match &command.phase {
                phase if is_writer_phase(phase) && validation_seen => {
                    return Err(invalid_configuration());
                }
                ValidationCommandPhase::Validation => validation_seen = true,
                _ => companion_paths.extend(command.allowed_companion_paths.iter()),
            }
        }
        if !validation_seen || companion_paths.len() > 20 {
            return Err(invalid_configuration());
        }
        profile_names.push(name);
    }
    profile_names.sort_unstable();
    if profile_names != ["affected", "changed", "fast", "final"]
        || referenced_commands.len() != commands.len()
    {
        return Err(invalid_configuration());
    }
    Ok(())
}

const fn is_writer_phase(phase: &ValidationCommandPhase) -> bool {
    matches!(
        phase,
        ValidationCommandPhase::Formatter
            | ValidationCommandPhase::SafeLintFix
            | ValidationCommandPhase::Codegen
            | ValidationCommandPhase::LockfileSync
    )
}

fn bounded_identifier(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 100
        && value.bytes().enumerate().all(|(index, byte)| {
            byte.is_ascii_alphanumeric() || (index > 0 && matches!(byte, b'.' | b'_' | b'-'))
        })
}

fn digest_changed_paths(
    changed_paths: &[String],
) -> Result<Sha256Digest, ValidationConfigurationError> {
    let mut paths = changed_paths.to_vec();
    paths.sort();
    if paths.windows(2).any(|pair| pair[0] == pair[1])
        || paths.iter().any(|path| !portable_relative_path(path))
    {
        return Err(invalid_configuration());
    }
    Ok(digest_framed(
        b"winwincode.validation.changed-paths.v1",
        paths.iter().map(String::as_bytes),
    ))
}

fn suggested_profile(
    changed_paths: &[String],
) -> (ValidationProfileName, ValidationProfileSelectionReasonCode) {
    if changed_paths.is_empty() {
        return (
            ValidationProfileName::Changed,
            ValidationProfileSelectionReasonCode::NoChangedPaths,
        );
    }
    if changed_paths.iter().any(|path| is_lockfile(path)) {
        return (
            ValidationProfileName::Affected,
            ValidationProfileSelectionReasonCode::LockfileChanged,
        );
    }
    let mut languages = changed_paths.iter().filter_map(|path| {
        let extension = path.rsplit_once('.').map(|(_, extension)| extension)?;
        match extension {
            "ts" | "tsx" | "js" | "mjs" | "cjs" => Some("typescript"),
            "rs" => Some("rust"),
            "py" => Some("python"),
            _ => None,
        }
    });
    let first = languages.next();
    if first.is_some() && languages.any(|language| Some(language) != first) {
        (
            ValidationProfileName::Affected,
            ValidationProfileSelectionReasonCode::MixedLanguages,
        )
    } else {
        (
            ValidationProfileName::Changed,
            ValidationProfileSelectionReasonCode::SingleLanguage,
        )
    }
}

fn is_lockfile(path: &str) -> bool {
    matches!(
        path.rsplit('/').next().unwrap_or(path),
        "Cargo.lock"
            | "package-lock.json"
            | "pnpm-lock.yaml"
            | "yarn.lock"
            | "poetry.lock"
            | "uv.lock"
            | "requirements.txt"
    )
}

fn portable_working_directory(path: &str) -> bool {
    path == "." || portable_relative_path(path)
}

fn portable_relative_path(path: &str) -> bool {
    let candidate = std::path::Path::new(path);
    !path.is_empty()
        && path.len() <= 4096
        && !path.contains(['\\', ':', '\0', '<', '>', '"', '|', '?', '*'])
        && !path.bytes().any(|byte| byte.is_ascii_control())
        && path.split('/').all(portable_component)
        && !candidate.is_absolute()
        && candidate
            .components()
            .all(|component| matches!(component, Component::Normal(_)))
}

fn portable_component(component: &str) -> bool {
    if component.is_empty()
        || matches!(component, "." | "..")
        || component.eq_ignore_ascii_case(".git")
        || component.ends_with([' ', '.'])
    {
        return false;
    }
    let stem = component.split('.').next().unwrap_or(component);
    let upper = stem.to_ascii_uppercase();
    !matches!(upper.as_str(), "CON" | "PRN" | "AUX" | "NUL")
        && !upper
            .strip_prefix("COM")
            .or_else(|| upper.strip_prefix("LPT"))
            .is_some_and(|suffix| {
                matches!(suffix, "1" | "2" | "3" | "4" | "5" | "6" | "7" | "8" | "9")
            })
}

fn is_shell_program(program: &str) -> bool {
    matches!(
        program.rsplit('/').next().unwrap_or(program),
        "sh" | "bash" | "zsh" | "fish" | "cmd" | "cmd.exe" | "powershell" | "pwsh"
    )
}

fn parse_profile_name(value: &str) -> Option<ValidationProfileName> {
    match value {
        "changed" => Some(ValidationProfileName::Changed),
        "fast" => Some(ValidationProfileName::Fast),
        "affected" => Some(ValidationProfileName::Affected),
        "final" => Some(ValidationProfileName::Final),
        _ => None,
    }
}

const fn profile_name_text(value: &ValidationProfileName) -> &'static str {
    match value {
        ValidationProfileName::Changed => "changed",
        ValidationProfileName::Fast => "fast",
        ValidationProfileName::Affected => "affected",
        ValidationProfileName::Final => "final",
    }
}

fn digest_framed<'a>(domain: &[u8], values: impl IntoIterator<Item = &'a [u8]>) -> Sha256Digest {
    let mut hasher = Sha256::new();
    frame(&mut hasher, domain);
    for value in values {
        frame(&mut hasher, value);
    }
    Sha256Digest(format!("sha256:{:x}", hasher.finalize()))
}

fn frame(hasher: &mut Sha256, value: &[u8]) {
    hasher.update(u64::try_from(value.len()).unwrap_or(u64::MAX).to_be_bytes());
    hasher.update(value);
}

const fn invalid_toml() -> ValidationConfigurationError {
    ValidationConfigurationError::new(
        ValidationConfigurationErrorCode::InvalidToml,
        "validation configuration is not canonical TOML",
    )
}

const fn invalid_configuration() -> ValidationConfigurationError {
    ValidationConfigurationError::new(
        ValidationConfigurationErrorCode::InvalidConfiguration,
        "validation configuration violates the canonical policy",
    )
}

const fn unknown_profile() -> ValidationConfigurationError {
    ValidationConfigurationError::new(
        ValidationConfigurationErrorCode::UnknownProfile,
        "validation profile is not configured",
    )
}

const fn revision_mismatch() -> ValidationConfigurationError {
    ValidationConfigurationError::new(
        ValidationConfigurationErrorCode::RevisionMismatch,
        "validation receipt does not bind the proven workspace revision",
    )
}
