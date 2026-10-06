//! Tool capabilities for models routed through the product's canonical port.

use std::sync::Arc;

use codex_core_api::{AuthManager, SharedModelsManager};
use codex_http_client::HttpClientFactory;
use codex_models_manager::ModelsManagerConfig;
use codex_models_manager::manager::{ModelsManager, ModelsManagerFuture, RefreshStrategy};
use codex_protocol::config_types::CollaborationModeMask;
use codex_protocol::openai_models::{ApplyPatchToolType, ModelInfo, ModelPreset, ModelsResponse};
use tokio::sync::TryLockError;

pub(super) fn with_patch_tools(inner: SharedModelsManager) -> SharedModelsManager {
    Arc::new(KernelModelsManager(inner))
}

#[derive(Debug)]
struct KernelModelsManager(SharedModelsManager);

impl ModelsManager for KernelModelsManager {
    fn set_api_key_model_discovery_enabled(&self, enabled: bool) {
        self.0.set_api_key_model_discovery_enabled(enabled);
    }

    fn list_models(
        &self,
        strategy: RefreshStrategy,
        http: HttpClientFactory,
    ) -> ModelsManagerFuture<'_, Vec<ModelPreset>> {
        self.0.list_models(strategy, http)
    }

    fn raw_model_catalog(
        &self,
        strategy: RefreshStrategy,
        http: HttpClientFactory,
    ) -> ModelsManagerFuture<'_, ModelsResponse> {
        self.0.raw_model_catalog(strategy, http)
    }

    fn refresh_after_auth_change(&self, http: HttpClientFactory) -> ModelsManagerFuture<'_, ()> {
        self.0.refresh_after_auth_change(http)
    }

    fn get_remote_models(&self) -> ModelsManagerFuture<'_, Vec<ModelInfo>> {
        self.0.get_remote_models()
    }

    fn try_get_remote_models(&self) -> Result<Vec<ModelInfo>, TryLockError> {
        self.0.try_get_remote_models()
    }

    fn auth_manager(&self) -> Option<&AuthManager> {
        self.0.auth_manager()
    }

    fn build_available_models(&self, models: Vec<ModelInfo>) -> Vec<ModelPreset> {
        self.0.build_available_models(models)
    }

    fn list_collaboration_modes(&self) -> Vec<CollaborationModeMask> {
        self.0.list_collaboration_modes()
    }

    fn try_list_models(&self) -> Result<Vec<ModelPreset>, TryLockError> {
        self.0.try_list_models()
    }

    fn get_default_model<'a>(
        &'a self,
        model: &'a Option<String>,
        allow_fallback: bool,
        strategy: RefreshStrategy,
        http: HttpClientFactory,
    ) -> ModelsManagerFuture<'a, String> {
        self.0
            .get_default_model(model, allow_fallback, strategy, http)
    }

    fn get_model_info<'a>(
        &'a self,
        model: &'a str,
        config: &'a ModelsManagerConfig,
    ) -> ModelsManagerFuture<'a, ModelInfo> {
        Box::pin(async move {
            let mut info = self.0.get_model_info(model, config).await;
            // The canonical Provider adapters translate custom tools into
            // portable function schemas. Unknown model metadata must not
            // remove the native patch handler from a coding Session.
            if info.used_fallback_model_metadata && info.apply_patch_tool_type.is_none() {
                info.apply_patch_tool_type = Some(ApplyPatchToolType::Freeform);
            }
            info
        })
    }

    fn refresh_if_new_etag(
        &self,
        etag: String,
        http: HttpClientFactory,
    ) -> ModelsManagerFuture<'_, ()> {
        self.0.refresh_if_new_etag(etag, http)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use codex_models_manager::manager::StaticModelsManager;

    #[tokio::test]
    async fn patch_capability_preserves_authoritative_metadata_and_config_overrides() {
        let mut known = codex_models_manager::model_info::model_info_from_slug("known-model");
        known.context_window = Some(64_000);
        let inner: SharedModelsManager = Arc::new(StaticModelsManager::new(
            None,
            ModelsResponse {
                models: vec![known],
            },
        ));
        let manager = with_patch_tools(inner.clone());
        let config = ModelsManagerConfig {
            model_context_window: Some(48_000),
            ..ModelsManagerConfig::default()
        };
        assert_eq!(
            manager.get_model_info("known-model", &config).await,
            inner.get_model_info("known-model", &config).await
        );
        for model in [
            "glm-5.3-flash",
            "qwen3.8-flash",
            "deepseek-flash",
            "mimo-v2.6-pro",
        ] {
            let actual = manager.get_model_info(model, &config).await;
            let mut expected = inner.get_model_info(model, &config).await;
            expected.apply_patch_tool_type = Some(ApplyPatchToolType::Freeform);
            assert_eq!(actual, expected);
        }
    }
}
