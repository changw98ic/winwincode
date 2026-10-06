// SPDX-License-Identifier: Apache-2.0

//! Verified `OpenCode` Go routes. Remote configuration remains data.

use std::collections::BTreeMap;

use serde_json::Value;
use winwincode_api::generated::{DeviceProviderConfig, DeviceProviderProtocol};

use crate::DeviceProviderError;

pub const GO_CHAT_ENDPOINT: &str = "https://opencode.ai/inference/go/openai/v1/chat/completions";
pub const GO_MESSAGES_ENDPOINT: &str = "https://opencode.ai/inference/go/anthropic/v1/messages";
pub const GO_USAGE_ENDPOINT: &str = "https://opencode.ai/inference/go/v1/usage";
const GO_CHAT_BASE: &str = "https://opencode.ai/inference/go/openai/v1";
const CHAT_PACKAGE: &str = "@ai-sdk/openai-compatible";
const GO_MESSAGES_BASE: &str = "https://opencode.ai/inference/go/anthropic/v1";
const MESSAGES_PACKAGE: &str = "@ai-sdk/anthropic";

/// One immutable catalog for a supported official Go protocol.
#[derive(Clone, Debug)]
pub struct OpenCodeGoRoute {
    pub models: BTreeMap<String, String>,
    organization_id: String,
    protocol: DeviceProviderProtocol,
}

impl OpenCodeGoRoute {
    /// Extracts a catalog from the selected organization's authenticated config.
    ///
    /// # Errors
    /// Rejects foreign routes, mismatched organizations and malformed model identities.
    pub fn from_configuration(
        configuration: &Value,
        organization_id: &str,
    ) -> Result<Self, DeviceProviderError> {
        Self::from_configuration_for_model(configuration, organization_id, None)
    }

    /// Selects the authenticated model's official Go protocol, or defaults to Chat.
    ///
    /// # Errors
    /// Rejects unknown models and foreign, metered or unsupported protocol overrides.
    pub fn from_configuration_for_model(
        configuration: &Value,
        organization_id: &str,
        selected_model: Option<&str>,
    ) -> Result<Self, DeviceProviderError> {
        if !valid_text(organization_id, 200) {
            return Err(DeviceProviderError);
        }
        let provider = configuration
            .pointer("/config/provider/opencode-go")
            .and_then(Value::as_object)
            .ok_or(DeviceProviderError)?;
        if provider.get("api").and_then(Value::as_str) != Some(GO_CHAT_BASE)
            || provider.get("npm").and_then(Value::as_str) != Some(CHAT_PACKAGE)
            || provider
                .get("options")
                .and_then(|options| options.get("headers"))
                .and_then(|headers| headers.get("x-opencode-org-id"))
                .and_then(Value::as_str)
                != Some(organization_id)
        {
            return Err(DeviceProviderError);
        }
        let catalog = provider
            .get("models")
            .and_then(Value::as_object)
            .ok_or(DeviceProviderError)?;
        if catalog.len() > 100 {
            return Err(DeviceProviderError);
        }
        let protocol = match selected_model {
            Some(model) => model_protocol(catalog.get(model).ok_or(DeviceProviderError)?)
                .ok_or(DeviceProviderError)?,
            None => DeviceProviderProtocol::OpenaiChatCompletions,
        };
        let mut models = BTreeMap::new();
        for (id, model) in catalog {
            if !valid_text(id, 128) || !model.is_object() {
                return Err(DeviceProviderError);
            }
            if model.get("tool_call").and_then(Value::as_bool) != Some(true)
                || model.get("status").and_then(Value::as_str) == Some("deprecated")
            {
                continue;
            }
            if model_protocol(model).as_ref() != Some(&protocol) {
                continue;
            }
            let upstream = match model.get("id").filter(|value| !value.is_null()) {
                Some(value) => value.as_str().ok_or(DeviceProviderError)?,
                None => id,
            };
            if !valid_text(upstream, 128) {
                return Err(DeviceProviderError);
            }
            models.insert(id.clone(), upstream.to_owned());
        }
        if models.is_empty() {
            return Err(DeviceProviderError);
        }
        if selected_model.is_some_and(|model| !models.contains_key(model)) {
            return Err(DeviceProviderError);
        }
        Ok(Self {
            models,
            organization_id: organization_id.to_owned(),
            protocol,
        })
    }

    /// Makes a private Device connection with the verified upstream model IDs.
    ///
    /// # Errors
    /// Rejects invalid names or duplicate upstream model aliases.
    pub fn config(
        &self,
        provider_id: String,
        display_name: String,
    ) -> Result<DeviceProviderConfig, DeviceProviderError> {
        let config = DeviceProviderConfig {
            provider_id,
            display_name,
            endpoint: match self.protocol {
                DeviceProviderProtocol::AnthropicMessages => GO_MESSAGES_ENDPOINT,
                DeviceProviderProtocol::OpenaiChatCompletions => GO_CHAT_ENDPOINT,
                DeviceProviderProtocol::Canonical => return Err(DeviceProviderError),
            }
            .to_owned(),
            protocol: self.protocol.clone(),
            model_ids: self.models.values().cloned().collect(),
            enabled: true,
        };
        if !crate::valid_device_provider_config(&config) {
            return Err(DeviceProviderError);
        }
        Ok(config)
    }

    /// Creates identity headers from a fixed connection and conversation.
    ///
    /// # Errors
    /// Rejects invalid conversation identities. Remote headers are never forwarded.
    pub fn headers(
        &self,
        conversation_id: &str,
    ) -> Result<BTreeMap<String, String>, DeviceProviderError> {
        if !valid_text(conversation_id, 200) {
            return Err(DeviceProviderError);
        }
        Ok(BTreeMap::from([
            ("x-opencode-org-id".into(), self.organization_id.clone()),
            ("x-opencode-session".into(), conversation_id.to_owned()),
            (
                "User-Agent".into(),
                concat!("winwincode/", env!("CARGO_PKG_VERSION")).into(),
            ),
        ]))
    }
}

fn model_protocol(model: &Value) -> Option<DeviceProviderProtocol> {
    let Some(route) = model.get("provider").filter(|route| !route.is_null()) else {
        return Some(DeviceProviderProtocol::OpenaiChatCompletions);
    };
    match (route.get("api")?.as_str()?, route.get("npm")?.as_str()?) {
        (GO_CHAT_BASE, CHAT_PACKAGE) => Some(DeviceProviderProtocol::OpenaiChatCompletions),
        (GO_MESSAGES_BASE, MESSAGES_PACKAGE) => Some(DeviceProviderProtocol::AnthropicMessages),
        _ => None,
    }
}

pub(crate) fn valid_text(value: &str, max: usize) -> bool {
    !value.is_empty()
        && value.trim() == value
        && value.len() <= max
        && !value.chars().any(char::is_control)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn configuration() -> Value {
        json!({"config":{"provider":{"opencode-go":{
            "api":GO_CHAT_BASE,"npm":CHAT_PACKAGE,
            "options":{"apiKey":"remote-secret-must-not-be-used","headers":{"x-opencode-org-id":"org-a"}},
            "models":{"glm-5.3-flash":{"tool_call":true},
                "other":{"tool_call":true,"provider":{"api":"https://untrusted.test","npm":CHAT_PACKAGE}}}
        }}}})
    }

    #[test]
    fn verified_go_catalog_and_headers_exclude_remote_credentials() {
        let route = OpenCodeGoRoute::from_configuration(&configuration(), "org-a").unwrap();
        let config = route.config("go-a".into(), "Go A".into()).unwrap();
        assert_eq!(config.model_ids, ["glm-5.3-flash"]);
        assert_eq!(config.endpoint, GO_CHAT_ENDPOINT);
        let headers = route.headers("conversation-a").unwrap();
        assert_eq!(headers["x-opencode-org-id"], "org-a");
        assert_eq!(headers["x-opencode-session"], "conversation-a");
        assert!(!format!("{route:?}{headers:?}").contains("remote-secret"));
        assert!(route.headers("unsafe\r\nconversation").is_err());
    }

    #[test]
    fn selected_qwen_uses_only_the_official_go_messages_catalog() {
        let mut value = configuration();
        value["config"]["provider"]["opencode-go"]["models"]["qwen3.8-flash"] = json!({
            "tool_call":true,"provider":{"api":GO_MESSAGES_BASE,"npm":MESSAGES_PACKAGE}
        });
        let chat = OpenCodeGoRoute::from_configuration(&value, "org-a").unwrap();
        assert_eq!(chat.models.keys().collect::<Vec<_>>(), ["glm-5.3-flash"]);
        let route =
            OpenCodeGoRoute::from_configuration_for_model(&value, "org-a", Some("qwen3.8-flash"))
                .unwrap();
        let config = route.config("go-qwen".into(), "Qwen".into()).unwrap();
        assert_eq!(config.endpoint, GO_MESSAGES_ENDPOINT);
        assert_eq!(config.protocol, DeviceProviderProtocol::AnthropicMessages);
        assert_eq!(config.model_ids, ["qwen3.8-flash"]);
        assert!(winwincode_api::opencode::valid_opencode_provider_route(
            &config
        ));
        assert!(
            OpenCodeGoRoute::from_configuration_for_model(&value, "org-a", Some("unknown"))
                .is_err()
        );
        for api in [
            "https://opencode.ai/inference/anthropic/v1",
            "https://evil.test/inference/go/anthropic/v1",
            "https://opencode.ai/inference/go/anthropic/v1?token=leak",
        ] {
            value["config"]["provider"]["opencode-go"]["models"]["qwen3.8-flash"]["provider"]["api"] =
                json!(api);
            assert!(
                OpenCodeGoRoute::from_configuration_for_model(
                    &value,
                    "org-a",
                    Some("qwen3.8-flash")
                )
                .is_err()
            );
        }
    }

    #[test]
    fn foreign_or_metered_routes_and_mismatched_organizations_fail_closed() {
        for api in [
            "https://opencode.ai/inference/openai/v1",
            "https://evil.test/inference/go/openai/v1",
            "http://opencode.ai/inference/go/openai/v1",
            "https://opencode.ai:8443/inference/go/openai/v1",
            "https://opencode.ai/inference/go/openai/v1?token=leak",
        ] {
            let mut value = configuration();
            value["config"]["provider"]["opencode-go"]["api"] = json!(api);
            assert!(OpenCodeGoRoute::from_configuration(&value, "org-a").is_err());
        }
        assert!(OpenCodeGoRoute::from_configuration(&configuration(), "org-b").is_err());
    }
}
