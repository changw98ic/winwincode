// SPDX-License-Identifier: Apache-2.0

use winwincode_control_plane::{
    ModelCapabilityOrigin, ModelCapabilitySnapshot, ModelCapabilitySource, ModelCatalogResolution,
    ModelCatalogService, ModelToolSupport, ProviderPresetsErrorKind, ResolvedEndpoint,
    find_provider_preset, list_provider_presets, resolve_provider_endpoint,
    validate_custom_endpoint,
};

const EXPECTED_PRESET_IDS: [&str; 8] = [
    "alibaba-qwen",
    "deepseek",
    "mistral",
    "moonshot-kimi",
    "openai",
    "openrouter",
    "xai-grok",
    "zhipu-glm",
];

/// Same identifier charset as the durable Provider catalog validators.
fn is_catalog_token(value: &str) -> bool {
    !value.is_empty()
        && value.bytes().all(|byte| {
            byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'_' | b':' | b'/' | b'-')
        })
}

#[derive(Clone, Copy, Debug)]
struct ConfirmedSource {
    provider_id: &'static str,
    model_id: &'static str,
    snapshot: ModelCapabilitySnapshot,
}

impl ModelCapabilitySource for ConfirmedSource {
    fn capability(&self, provider_id: &str, model_id: &str) -> Option<ModelCapabilitySnapshot> {
        if provider_id == self.provider_id && model_id == self.model_id {
            Some(self.snapshot)
        } else {
            None
        }
    }
}

fn catalog_snapshot() -> ModelCapabilitySnapshot {
    ModelCapabilitySnapshot {
        context_window_tokens: 128_000,
        max_output_tokens: 16_384,
        tool_support: ModelToolSupport::Parallel,
        origin: ModelCapabilityOrigin::ProviderCatalog,
    }
}

#[test]
fn list_presets_is_deterministic_sorted_and_template_shaped() {
    let first = list_provider_presets().expect("preset listing must succeed");
    let second = list_provider_presets().expect("preset listing must succeed");
    assert_eq!(first, second, "preset listing must be deterministic");

    let ids: Vec<_> = first
        .iter()
        .map(|preset| preset.provider_id.as_str())
        .collect();
    assert_eq!(
        ids, EXPECTED_PRESET_IDS,
        "presets must stay sorted and complete"
    );

    for preset in &first {
        assert!(
            preset.base_url.starts_with("https://"),
            "preset endpoints must be https"
        );
        assert!(
            preset.docs_url.starts_with("https://"),
            "preset docs must be https"
        );
        assert_eq!(preset.adapter_kind, "openai-responses");
        assert!(!preset.display_name.is_empty());
        assert!(
            !preset.models.is_empty(),
            "every preset documents at least one model"
        );
        assert!(
            is_catalog_token(&preset.provider_id),
            "provider id must fit catalog token rules"
        );
        let mut model_ids: Vec<_> = preset
            .models
            .iter()
            .map(|model| model.model_id.as_str())
            .collect();
        let model_count = model_ids.len();
        model_ids.sort_unstable();
        model_ids.dedup();
        assert_eq!(
            model_ids.len(),
            model_count,
            "preset model ids must be unique"
        );
        for model_id in model_ids {
            assert!(
                is_catalog_token(model_id),
                "model id must fit catalog token rules"
            );
        }
        for model in &preset.models {
            let serialized = serde_json::to_string(model).expect("preset model must serialize");
            assert!(
                !serialized.contains("contextWindow")
                    && !serialized.contains("maxOutput")
                    && !serialized.contains("toolSupport"),
                "presets are identity templates and must not carry capability fields"
            );
        }
    }
}

#[test]
fn presets_report_unknown_capabilities_without_catalog_or_probe_data() {
    let sources: [&dyn ModelCapabilitySource; 0] = [];
    let service = ModelCatalogService::new(&sources);

    let entries = service
        .list_preset_models("openai")
        .expect("preset model listing must succeed");
    assert!(!entries.is_empty(), "the openai preset documents models");
    for entry in &entries {
        assert!(
            entry.capability.is_none(),
            "capability must stay unknown without a source"
        );
    }

    let resolution = service
        .resolve_model("openai", "gpt-5")
        .expect("preset model resolution must succeed");
    assert!(!resolution.capability_is_known());
    assert!(resolution.capability.is_none());
    assert_eq!(resolution.display_name, "GPT-5");

    let serialized = serde_json::to_string(&entries).expect("entries must serialize");
    assert!(
        !serialized.contains("contextWindowTokens")
            && !serialized.contains("maxOutputTokens")
            && !serialized.contains("toolSupport"),
        "unknown capabilities must be absent, never defaulted"
    );
}

#[test]
fn capabilities_come_only_from_catalog_or_probe_sources() {
    let catalog = ConfirmedSource {
        provider_id: "deepseek",
        model_id: "deepseek-chat",
        snapshot: catalog_snapshot(),
    };
    let probe = ConfirmedSource {
        provider_id: "deepseek",
        model_id: "deepseek-reasoner",
        snapshot: ModelCapabilitySnapshot {
            context_window_tokens: 64_000,
            max_output_tokens: 8_192,
            tool_support: ModelToolSupport::Serial,
            origin: ModelCapabilityOrigin::ConnectionProbe,
        },
    };
    let sources: [&dyn ModelCapabilitySource; 2] = [&catalog, &probe];
    let service = ModelCatalogService::new(&sources);

    let from_catalog = service
        .resolve_model("deepseek", "deepseek-chat")
        .expect("catalog-backed resolution must succeed");
    let capability = from_catalog
        .capability
        .expect("catalog source must confirm");
    assert_eq!(capability.origin, ModelCapabilityOrigin::ProviderCatalog);
    assert_eq!(capability.context_window_tokens, 128_000);
    assert_eq!(capability.max_output_tokens, 16_384);
    assert_eq!(capability.tool_support, ModelToolSupport::Parallel);

    let from_probe = service
        .resolve_model("deepseek", "deepseek-reasoner")
        .expect("probe-backed resolution must succeed");
    assert_eq!(
        from_probe.capability.map(|snapshot| snapshot.origin),
        Some(ModelCapabilityOrigin::ConnectionProbe)
    );

    let unconfirmed = service
        .resolve_model("deepseek", "deepseek-unknown")
        .expect("unconfirmed model stays a valid identity");
    assert!(
        unconfirmed.capability.is_none(),
        "unconfirmed models report unknown"
    );

    let custom = service
        .resolve_model("team-gateway.corp", "team-model")
        .expect("custom provider identity must resolve");
    assert!(custom.capability.is_none());
    assert_eq!(
        custom.display_name, "team-model",
        "identity fallback is the model id"
    );
}

#[test]
fn first_configured_source_wins_for_the_same_model() {
    let first = ConfirmedSource {
        provider_id: "openai",
        model_id: "gpt-5",
        snapshot: catalog_snapshot(),
    };
    let second = ConfirmedSource {
        provider_id: "openai",
        model_id: "gpt-5",
        snapshot: ModelCapabilitySnapshot {
            context_window_tokens: 1,
            max_output_tokens: 1,
            tool_support: ModelToolSupport::Unsupported,
            origin: ModelCapabilityOrigin::ConnectionProbe,
        },
    };
    let sources: [&dyn ModelCapabilitySource; 2] = [&first, &second];
    let service = ModelCatalogService::new(&sources);
    let resolution = service
        .resolve_model("openai", "gpt-5")
        .expect("resolution must succeed");
    assert_eq!(resolution.capability, Some(catalog_snapshot()));
}

#[test]
fn list_preset_models_rejects_unknown_and_malformed_providers() {
    let sources: [&dyn ModelCapabilitySource; 0] = [];
    let service = ModelCatalogService::new(&sources);

    let unknown = service
        .list_preset_models("not-a-preset")
        .expect_err("unknown provider");
    assert_eq!(unknown.kind(), ProviderPresetsErrorKind::ProviderNotFound);

    let malformed = service
        .list_preset_models("bad id!")
        .expect_err("malformed provider");
    assert_eq!(malformed.kind(), ProviderPresetsErrorKind::InvalidRequest);

    let malformed_model = service
        .resolve_model("openai", "bad model!")
        .expect_err("malformed model");
    assert_eq!(
        malformed_model.kind(),
        ProviderPresetsErrorKind::InvalidRequest
    );
}

#[test]
fn custom_endpoint_accepts_canonical_https() {
    for accepted in [
        "https://api.example.com/v1",
        "https://api.example.com",
        "https://localhost:8443/v1",
        "https://gateway.internal.corp/v1/openai",
        "https://api.example.com/v1/",
        "https://llm.team.example:8443/v1",
    ] {
        validate_custom_endpoint(accepted)
            .unwrap_or_else(|error| panic!("endpoint {accepted} must be accepted: {error}"));
    }
}

#[test]
fn custom_endpoint_rejects_insecure_credential_bearing_and_malformed_urls() {
    let long_path = "a".repeat(2_048 - "https://api.example.com/".len());
    let accepted_boundary = format!("https://api.example.com/{long_path}");
    assert_eq!(
        accepted_boundary.len(),
        2_048,
        "boundary stays within the bound"
    );
    validate_custom_endpoint(&accepted_boundary).expect("2,048-byte endpoint must be accepted");

    let too_long = format!("{accepted_boundary}a");
    for rejected in [
        "http://api.example.com/v1",
        "ftp://api.example.com",
        "https://user:credential@api.example.com/v1",
        "https://token@api.example.com/v1",
        "https://api.example.com/v1?key=value",
        "https://api.example.com/v1#fragment",
        " https://api.example.com/v1",
        "https://api.example.com/v1 ",
        "https://api.example.com/v1\n",
        "https://",
        "api.example.com/v1",
        "",
        too_long.as_str(),
    ] {
        let error = validate_custom_endpoint(rejected)
            .expect_err("endpoint must be rejected without echoing input");
        assert_eq!(
            error.kind(),
            ProviderPresetsErrorKind::InvalidRequest,
            "unexpectedly accepted: {rejected}"
        );
    }
}

#[test]
fn resolve_provider_endpoint_prefers_validated_custom_and_fails_closed() {
    let preset = resolve_provider_endpoint("deepseek", None).expect("preset endpoint must resolve");
    assert_eq!(
        preset,
        ResolvedEndpoint::Preset {
            endpoint: "https://api.deepseek.com/v1".to_owned(),
        }
    );
    assert!(!preset.is_custom());
    assert_eq!(preset.endpoint(), "https://api.deepseek.com/v1");

    let custom =
        resolve_provider_endpoint("deepseek", Some("https://llm.team.example/v1")).expect("custom");
    assert_eq!(
        custom,
        ResolvedEndpoint::Custom {
            endpoint: "https://llm.team.example/v1".to_owned(),
        }
    );
    assert!(custom.is_custom());

    let custom_without_preset =
        resolve_provider_endpoint("team-gateway", Some("https://gw.example/v1"))
            .expect("custom endpoints need no preset");
    assert_eq!(
        custom_without_preset,
        ResolvedEndpoint::Custom {
            endpoint: "https://gw.example/v1".to_owned(),
        }
    );

    let unknown = resolve_provider_endpoint("not-a-preset", None).expect_err("unknown provider");
    assert_eq!(unknown.kind(), ProviderPresetsErrorKind::ProviderNotFound);

    let invalid = resolve_provider_endpoint("deepseek", Some("http://api.example.com/v1"))
        .expect_err("invalid custom must fail closed, not fall back to the preset");
    assert_eq!(invalid.kind(), ProviderPresetsErrorKind::InvalidRequest);

    let leaky = resolve_provider_endpoint(
        "deepseek",
        Some("https://user:credential@api.example.com/v1"),
    )
    .expect_err("credential-bearing endpoint must be rejected");
    assert_eq!(leaky.kind(), ProviderPresetsErrorKind::InvalidRequest);

    let malformed = resolve_provider_endpoint("bad id!", None).expect_err("malformed provider");
    assert_eq!(malformed.kind(), ProviderPresetsErrorKind::InvalidRequest);
}

#[test]
fn find_provider_preset_round_trips_the_listing() {
    let presets = list_provider_presets().expect("preset listing must succeed");
    for expected in &presets {
        let found = find_provider_preset(&expected.provider_id).expect("preset must be found");
        assert_eq!(found, *expected);
        let service = ModelCatalogService::new(&[]);
        let entries = service
            .list_preset_models(&found.provider_id)
            .expect("preset models must list");
        let entry_ids: Vec<_> = entries
            .iter()
            .map(|entry| entry.model_id.as_str())
            .collect();
        let expected_ids: Vec<_> = found
            .models
            .iter()
            .map(|model| model.model_id.as_str())
            .collect();
        assert_eq!(
            entry_ids, expected_ids,
            "catalog join must preserve preset identity"
        );
    }

    let unknown = find_provider_preset("not-a-preset").expect_err("unknown preset");
    assert_eq!(unknown.kind(), ProviderPresetsErrorKind::ProviderNotFound);

    let malformed = find_provider_preset("bad id!").expect_err("malformed preset id");
    assert_eq!(malformed.kind(), ProviderPresetsErrorKind::InvalidRequest);
}

#[test]
fn preset_serialization_never_contains_credential_material() {
    let presets = list_provider_presets().expect("preset listing must succeed");
    let resolution = ModelCatalogResolution {
        provider_id: "deepseek".to_owned(),
        model_id: "deepseek-chat".to_owned(),
        display_name: "DeepSeek Chat".to_owned(),
        capability: Some(catalog_snapshot()),
    };
    let endpoint = ResolvedEndpoint::Custom {
        endpoint: "https://api.example.com/v1".to_owned(),
    };

    let serialized = [
        serde_json::to_string(&presets).expect("presets must serialize"),
        serde_json::to_string(&resolution).expect("resolution must serialize"),
        serde_json::to_string(&endpoint).expect("endpoint must serialize"),
    ]
    .join("\n");

    for fragment in [
        "sk-",
        "Bearer ",
        "bearer ",
        "api_key",
        "apikey",
        "api-key",
        "authorization",
        "password",
        "secret",
        "credential=",
    ] {
        assert!(
            !serialized.contains(fragment),
            "preset serialization must stay free of credential material"
        );
    }
}
