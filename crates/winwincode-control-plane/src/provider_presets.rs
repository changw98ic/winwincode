// SPDX-License-Identifier: Apache-2.0

//! Discoverable Provider presets and the model catalog service surface.
//!
//! A preset is a configuration template only: a stable Provider identifier, a
//! display name, a documented `OpenAI`-compatible base URL, and the Provider's
//! documented model identifiers. Presets never carry or fabricate model
//! capabilities (context window, tool support, billing). Real capabilities
//! come exclusively from an authoritative [`ModelCapabilitySource`] — the
//! durable [`crate::provider_catalog`] today, or a live connection probe in
//! later integration — and [`ModelCatalogService`] reports unknown
//! capabilities rather than inventing defaults.
//!
//! Custom endpoints are accepted only after the same canonical HTTPS
//! validation used by the Provider HTTPS adapter: the endpoint must be a
//! trimmed `https` URI without embedded userinfo, query, or fragment, so a
//! credential can never hide inside the URL. This module never accepts,
//! stores, or emits credential material, and every serializable output passes
//! the [`crate::credential_leak_gate`].
//!
//! # MODEL-100.3 integration boundary
//!
//! Credential write/test/rotate APIs should (1) implement
//! [`ModelCapabilitySource`] over the durable
//! [`crate::provider_catalog::ProviderCatalogService`] projection or a live
//! connection probe, and (2) pair a preset with a
//! [`winwincode_domain::CredentialReferenceId`] to build the
//! [`crate::provider_catalog::ProviderDescriptor`] accepted by
//! [`crate::provider_catalog::ProviderCatalogService::upsert`]. A descriptor
//! requires positive capability values, so a preset alone can never satisfy
//! it: Provider registration stays gated behind verified capability data by
//! design. The client settings surface can consume [`list_provider_presets`],
//! [`find_provider_preset`], and [`resolve_provider_endpoint`] to replace
//! hand-typed Provider/model input; UI wiring is intentionally out of scope
//! for this module.

use std::fmt;

use serde::{Deserialize, Serialize};

use crate::credential_leak_gate::{
    CredentialLeakError, CredentialLeakGate, CredentialOutputBoundary,
};
use crate::provider_catalog::ModelToolSupport;
use crate::provider_https_sse::{MAX_ENDPOINT_BYTES, canonical_https_endpoint};

/// Adapter implementation every preset routes through: the embedded Kernel
/// `OpenAI`-compatible execution adapter.
const PRESET_ADAPTER_KIND: &str = "openai-responses";
/// Same Provider identifier bound as the durable Provider catalog.
const MAX_PROVIDER_ID_CHARS: usize = 128;
/// Same model identifier bound as the durable Provider catalog.
const MAX_MODEL_ID_CHARS: usize = 200;

/// Bounded Provider preset and model catalog failure categories.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ProviderPresetsErrorKind {
    InvalidRequest,
    ProviderNotFound,
    CredentialLeak,
}

/// Bounded error that never copies preset endpoint or model input.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ProviderPresetsError {
    kind: ProviderPresetsErrorKind,
    message: &'static str,
}

impl ProviderPresetsError {
    const fn new(kind: ProviderPresetsErrorKind, message: &'static str) -> Self {
        Self { kind, message }
    }

    const fn invalid() -> Self {
        Self::new(
            ProviderPresetsErrorKind::InvalidRequest,
            "Provider preset request is invalid",
        )
    }

    const fn provider_not_found() -> Self {
        Self::new(
            ProviderPresetsErrorKind::ProviderNotFound,
            "Provider was not found among the presets",
        )
    }

    const fn credential_leak() -> Self {
        Self::new(
            ProviderPresetsErrorKind::CredentialLeak,
            "Provider preset output was rejected by the Credential leak gate",
        )
    }

    /// Returns the stable machine-readable failure category.
    #[must_use]
    pub const fn kind(&self) -> ProviderPresetsErrorKind {
        self.kind
    }
}

impl fmt::Display for ProviderPresetsError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.message)
    }
}

impl std::error::Error for ProviderPresetsError {}

impl From<CredentialLeakError> for ProviderPresetsError {
    fn from(_error: CredentialLeakError) -> Self {
        Self::credential_leak()
    }
}

/// One documented model identifier inside a preset.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct PresetModel {
    pub model_id: String,
    pub display_name: String,
}

/// A discoverable Provider configuration template.
///
/// Identity and documented defaults only: `models` lists identifiers to
/// pre-fill the model picker and carries no capability values.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ProviderPreset {
    pub provider_id: String,
    pub display_name: String,
    /// Documented default `OpenAI`-compatible base URL.
    pub base_url: String,
    /// Stable adapter implementation identifier, never adapter configuration.
    pub adapter_kind: String,
    /// Provider documentation that publishes the base URL and model catalog.
    pub docs_url: String,
    pub models: Vec<PresetModel>,
}

/// Where a resolved endpoint came from.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum EndpointSource {
    Preset,
    Custom,
}

/// The endpoint a settings flow should use after safe validation.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case", deny_unknown_fields)]
pub enum ResolvedEndpoint {
    Preset { endpoint: String },
    Custom { endpoint: String },
}

impl ResolvedEndpoint {
    /// Returns the endpoint URL regardless of its source.
    #[must_use]
    pub fn endpoint(&self) -> &str {
        match self {
            Self::Preset { endpoint } | Self::Custom { endpoint } => endpoint,
        }
    }

    /// Returns whether the endpoint came from a user-supplied custom entry.
    #[must_use]
    pub const fn is_custom(&self) -> bool {
        matches!(self, Self::Custom { .. })
    }
}

/// Which authoritative source confirmed one capability snapshot.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ModelCapabilityOrigin {
    /// Confirmed against the durable Provider catalog projection.
    ProviderCatalog,
    /// Confirmed through a live connection probe of the endpoint.
    ConnectionProbe,
}

/// Real capability values confirmed by an authoritative source.
///
/// The snapshot intentionally carries no billing fields: billing facts stay
/// with the Provider catalog and gateway settlement, never with presets.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ModelCapabilitySnapshot {
    pub context_window_tokens: u64,
    pub max_output_tokens: u64,
    pub tool_support: ModelToolSupport,
    pub origin: ModelCapabilityOrigin,
}

/// Port implemented by authoritative capability sources.
///
/// Implementations return `Some` only for values verified against the durable
/// Provider catalog or a live connection probe. Verified values are positive,
/// `max_output_tokens` never exceeds `context_window_tokens`, and no value is
/// ever defaulted from a preset: the presets module holds identity data only.
pub trait ModelCapabilitySource {
    fn capability(&self, provider_id: &str, model_id: &str) -> Option<ModelCapabilitySnapshot>;
}

/// One documented preset model joined to its authoritative capability.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ModelCatalogModelEntry {
    pub model_id: String,
    pub display_name: String,
    /// `None` means unknown: no catalog or probe data confirmed a value.
    pub capability: Option<ModelCapabilitySnapshot>,
}

/// One model resolution with preset identity and authoritative capability.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ModelCatalogResolution {
    pub provider_id: String,
    pub model_id: String,
    pub display_name: String,
    /// `None` means unknown: no catalog or probe data confirmed a value.
    pub capability: Option<ModelCapabilitySnapshot>,
}

impl ModelCatalogResolution {
    /// Returns whether an authoritative source confirmed the capabilities.
    #[must_use]
    pub const fn capability_is_known(&self) -> bool {
        self.capability.is_some()
    }
}

/// Joins preset identity with authoritative model capabilities.
///
/// The service owns no capability data: it forwards every capability lookup
/// to the configured [`ModelCapabilitySource`] list in order and reports the
/// first confirmed value, or unknown.
#[derive(Clone, Copy)]
pub struct ModelCatalogService<'a> {
    sources: &'a [&'a dyn ModelCapabilitySource],
}

impl fmt::Debug for ModelCatalogService<'_> {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("ModelCatalogService")
            .field("source_count", &self.sources.len())
            .finish()
    }
}

impl<'a> ModelCatalogService<'a> {
    #[must_use]
    pub fn new(sources: &'a [&'a dyn ModelCapabilitySource]) -> Self {
        Self { sources }
    }

    /// Lists every documented model of one preset with its capability status.
    ///
    /// # Errors
    ///
    /// Rejects a malformed Provider identifier or an unknown preset.
    pub fn list_preset_models(
        &self,
        provider_id: &str,
    ) -> Result<Vec<ModelCatalogModelEntry>, ProviderPresetsError> {
        validate_provider_id(provider_id)?;
        let template =
            find_template(provider_id).ok_or_else(ProviderPresetsError::provider_not_found)?;
        let entries = template
            .models
            .iter()
            .map(|model| ModelCatalogModelEntry {
                model_id: model.model_id.to_owned(),
                display_name: model.display_name.to_owned(),
                capability: self.confirmed_capability(provider_id, model.model_id),
            })
            .collect();
        checked_serializable(&entries)?;
        Ok(entries)
    }

    /// Resolves one model identity with its authoritative capability.
    ///
    /// Works for preset Providers and custom endpoint Providers alike; a
    /// Provider outside the presets keeps its caller-supplied identity and
    /// still reports unknown capabilities until a source confirms values.
    ///
    /// # Errors
    ///
    /// Rejects malformed Provider or model identifiers.
    pub fn resolve_model(
        &self,
        provider_id: &str,
        model_id: &str,
    ) -> Result<ModelCatalogResolution, ProviderPresetsError> {
        validate_provider_id(provider_id)?;
        validate_model_id(model_id)?;
        let display_name = find_template(provider_id)
            .and_then(|template| {
                template
                    .models
                    .iter()
                    .find(|model| model.model_id == model_id)
            })
            .map_or_else(
                || model_id.to_owned(),
                |model| model.display_name.to_owned(),
            );
        let resolution = ModelCatalogResolution {
            provider_id: provider_id.to_owned(),
            model_id: model_id.to_owned(),
            display_name,
            capability: self.confirmed_capability(provider_id, model_id),
        };
        checked_serializable(&resolution)?;
        Ok(resolution)
    }

    fn confirmed_capability(
        &self,
        provider_id: &str,
        model_id: &str,
    ) -> Option<ModelCapabilitySnapshot> {
        self.sources
            .iter()
            .find_map(|source| source.capability(provider_id, model_id))
    }
}

/// Lists every built-in Provider preset, sorted by Provider identifier.
///
/// # Errors
///
/// Fails closed when the leak gate rejects preset serialization.
pub fn list_provider_presets() -> Result<Vec<ProviderPreset>, ProviderPresetsError> {
    let presets = BUILTIN_PROVIDER_PRESETS
        .iter()
        .map(preset_from_template)
        .collect();
    checked_serializable(&presets)?;
    Ok(presets)
}

/// Returns one built-in Provider preset by its exact identifier.
///
/// # Errors
///
/// Rejects a malformed identifier or an unknown preset.
pub fn find_provider_preset(provider_id: &str) -> Result<ProviderPreset, ProviderPresetsError> {
    validate_provider_id(provider_id)?;
    let template =
        find_template(provider_id).ok_or_else(ProviderPresetsError::provider_not_found)?;
    let preset = preset_from_template(template);
    checked_serializable(&preset)?;
    Ok(preset)
}

/// Resolves the endpoint for one Provider, preferring a validated custom
/// entry over the preset default.
///
/// A custom entry is returned only after [`validate_custom_endpoint`]
/// accepts it; an invalid custom entry fails closed instead of falling back
/// to the preset default. A custom entry never requires a preset Provider,
/// because custom endpoints exist precisely for unlisted Providers.
///
/// # Errors
///
/// Rejects an invalid custom endpoint, a malformed Provider identifier, or a
/// Provider without a preset when no custom entry was supplied.
pub fn resolve_provider_endpoint(
    provider_id: &str,
    custom_endpoint: Option<&str>,
) -> Result<ResolvedEndpoint, ProviderPresetsError> {
    validate_provider_id(provider_id)?;
    if let Some(endpoint) = custom_endpoint {
        validate_custom_endpoint(endpoint)?;
        let resolved = ResolvedEndpoint::Custom {
            endpoint: endpoint.to_owned(),
        };
        checked_serializable(&resolved)?;
        return Ok(resolved);
    }
    let template =
        find_template(provider_id).ok_or_else(ProviderPresetsError::provider_not_found)?;
    let resolved = ResolvedEndpoint::Preset {
        endpoint: template.base_url.to_owned(),
    };
    checked_serializable(&resolved)?;
    Ok(resolved)
}

/// Accepts a custom `OpenAI`-compatible endpoint only after the same
/// canonical HTTPS validation the Provider HTTPS adapter applies: a trimmed
/// `https` URI with a host, no embedded userinfo, no query, and no fragment.
///
/// Embedded userinfo is rejected outright, so a credential can never be
/// smuggled through the endpoint string.
///
/// # Errors
///
/// Returns [`ProviderPresetsErrorKind::InvalidRequest`] for every rejected
/// shape without echoing the input.
pub fn validate_custom_endpoint(endpoint: &str) -> Result<(), ProviderPresetsError> {
    if valid_preset_endpoint(endpoint) {
        Ok(())
    } else {
        Err(ProviderPresetsError::invalid())
    }
}

struct PresetModelTemplate {
    model_id: &'static str,
    display_name: &'static str,
}

struct ProviderPresetTemplate {
    provider_id: &'static str,
    display_name: &'static str,
    base_url: &'static str,
    docs_url: &'static str,
    models: &'static [PresetModelTemplate],
}

/// Built-in presets, kept sorted by Provider identifier.
///
/// Base URLs and model identifiers mirror each Provider's published
/// `OpenAI`-compatible documentation (see `docs_url`); they are configuration
/// templates and deliberately exclude capability and billing values.
const BUILTIN_PROVIDER_PRESETS: &[ProviderPresetTemplate] = &[
    ProviderPresetTemplate {
        provider_id: "alibaba-qwen",
        display_name: "Alibaba Cloud Qwen",
        base_url: "https://dashscope.aliyuncs.com/compatible-mode/v1",
        docs_url: "https://www.alibabacloud.com/help/en/model-studio",
        models: &[
            PresetModelTemplate {
                model_id: "qwen-max",
                display_name: "Qwen Max",
            },
            PresetModelTemplate {
                model_id: "qwen-plus",
                display_name: "Qwen Plus",
            },
            PresetModelTemplate {
                model_id: "qwen-flash",
                display_name: "Qwen Flash",
            },
            PresetModelTemplate {
                model_id: "qwen3-coder-plus",
                display_name: "Qwen3 Coder Plus",
            },
        ],
    },
    ProviderPresetTemplate {
        provider_id: "deepseek",
        display_name: "DeepSeek",
        base_url: "https://api.deepseek.com/v1",
        docs_url: "https://api-docs.deepseek.com",
        models: &[
            PresetModelTemplate {
                model_id: "deepseek-chat",
                display_name: "DeepSeek Chat",
            },
            PresetModelTemplate {
                model_id: "deepseek-reasoner",
                display_name: "DeepSeek Reasoner",
            },
        ],
    },
    ProviderPresetTemplate {
        provider_id: "mistral",
        display_name: "Mistral AI",
        base_url: "https://api.mistral.ai/v1",
        docs_url: "https://docs.mistral.ai",
        models: &[
            PresetModelTemplate {
                model_id: "mistral-large-latest",
                display_name: "Mistral Large",
            },
            PresetModelTemplate {
                model_id: "mistral-medium-latest",
                display_name: "Mistral Medium",
            },
            PresetModelTemplate {
                model_id: "mistral-small-latest",
                display_name: "Mistral Small",
            },
            PresetModelTemplate {
                model_id: "codestral-latest",
                display_name: "Codestral",
            },
        ],
    },
    ProviderPresetTemplate {
        provider_id: "moonshot-kimi",
        display_name: "Moonshot AI Kimi",
        base_url: "https://api.moonshot.ai/v1",
        docs_url: "https://platform.kimi.ai/docs",
        models: &[
            PresetModelTemplate {
                model_id: "kimi-k2-0905-preview",
                display_name: "Kimi K2 0905 Preview",
            },
            PresetModelTemplate {
                model_id: "kimi-k2-turbo-preview",
                display_name: "Kimi K2 Turbo Preview",
            },
        ],
    },
    ProviderPresetTemplate {
        provider_id: "openai",
        display_name: "OpenAI",
        base_url: "https://api.openai.com/v1",
        docs_url: "https://platform.openai.com/docs/models",
        models: &[
            PresetModelTemplate {
                model_id: "gpt-5",
                display_name: "GPT-5",
            },
            PresetModelTemplate {
                model_id: "gpt-5-mini",
                display_name: "GPT-5 mini",
            },
            PresetModelTemplate {
                model_id: "gpt-5-nano",
                display_name: "GPT-5 nano",
            },
            PresetModelTemplate {
                model_id: "gpt-4.1",
                display_name: "GPT-4.1",
            },
            PresetModelTemplate {
                model_id: "gpt-4.1-mini",
                display_name: "GPT-4.1 mini",
            },
            PresetModelTemplate {
                model_id: "gpt-4o",
                display_name: "GPT-4o",
            },
            PresetModelTemplate {
                model_id: "gpt-4o-mini",
                display_name: "GPT-4o mini",
            },
            PresetModelTemplate {
                model_id: "o3",
                display_name: "o3",
            },
            PresetModelTemplate {
                model_id: "o4-mini",
                display_name: "o4-mini",
            },
        ],
    },
    ProviderPresetTemplate {
        provider_id: "openrouter",
        display_name: "OpenRouter",
        base_url: "https://openrouter.ai/api/v1",
        docs_url: "https://openrouter.ai/docs",
        models: &[PresetModelTemplate {
            model_id: "openrouter/auto",
            display_name: "OpenRouter Auto",
        }],
    },
    ProviderPresetTemplate {
        provider_id: "xai-grok",
        display_name: "xAI Grok",
        base_url: "https://api.x.ai/v1",
        docs_url: "https://docs.x.ai",
        models: &[
            PresetModelTemplate {
                model_id: "grok-4",
                display_name: "Grok 4",
            },
            PresetModelTemplate {
                model_id: "grok-code-fast-1",
                display_name: "Grok Code Fast",
            },
            PresetModelTemplate {
                model_id: "grok-3",
                display_name: "Grok 3",
            },
            PresetModelTemplate {
                model_id: "grok-3-mini",
                display_name: "Grok 3 mini",
            },
        ],
    },
    ProviderPresetTemplate {
        provider_id: "zhipu-glm",
        display_name: "Z.ai GLM",
        base_url: "https://open.bigmodel.cn/api/paas/v4",
        docs_url: "https://docs.bigmodel.cn",
        models: &[
            PresetModelTemplate {
                model_id: "glm-4.6",
                display_name: "GLM-4.6",
            },
            PresetModelTemplate {
                model_id: "glm-4.5",
                display_name: "GLM-4.5",
            },
            PresetModelTemplate {
                model_id: "glm-4.5-air",
                display_name: "GLM-4.5 Air",
            },
            PresetModelTemplate {
                model_id: "glm-4.5-flash",
                display_name: "GLM-4.5 Flash",
            },
        ],
    },
];

fn preset_from_template(template: &ProviderPresetTemplate) -> ProviderPreset {
    ProviderPreset {
        provider_id: template.provider_id.to_owned(),
        display_name: template.display_name.to_owned(),
        base_url: template.base_url.to_owned(),
        adapter_kind: PRESET_ADAPTER_KIND.to_owned(),
        docs_url: template.docs_url.to_owned(),
        models: template
            .models
            .iter()
            .map(|model| PresetModel {
                model_id: model.model_id.to_owned(),
                display_name: model.display_name.to_owned(),
            })
            .collect(),
    }
}

fn find_template(provider_id: &str) -> Option<&'static ProviderPresetTemplate> {
    BUILTIN_PROVIDER_PRESETS
        .iter()
        .find(|preset| preset.provider_id == provider_id)
}

fn checked_serializable<T: Serialize + ?Sized>(value: &T) -> Result<(), ProviderPresetsError> {
    CredentialLeakGate::default()
        .inspect_serializable(CredentialOutputBoundary::Serialization, value)?;
    Ok(())
}

fn validate_provider_id(value: &str) -> Result<(), ProviderPresetsError> {
    validate_token(value, MAX_PROVIDER_ID_CHARS)
}

fn validate_model_id(value: &str) -> Result<(), ProviderPresetsError> {
    validate_token(value, MAX_MODEL_ID_CHARS)
}

/// Same identifier charset as the durable Provider catalog validators.
fn validate_token(value: &str, max_chars: usize) -> Result<(), ProviderPresetsError> {
    if value.is_empty()
        || value.len() > max_chars
        || !value.bytes().all(|byte| {
            byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'_' | b':' | b'/' | b'-')
        })
    {
        Err(ProviderPresetsError::invalid())
    } else {
        Ok(())
    }
}

/// Preset gate over the authority-owned canonical HTTPS endpoint check, with
/// the shared 2,048-byte bound and a fragment/query pre-filter.
fn valid_preset_endpoint(value: &str) -> bool {
    value.len() <= MAX_ENDPOINT_BYTES
        && value.trim() == value
        && !value.chars().any(char::is_control)
        && !value.contains(['?', '#'])
        && canonical_https_endpoint(value)
}
