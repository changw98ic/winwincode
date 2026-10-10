use std::collections::HashMap;
use std::fmt;
use std::sync::Arc;

use futures::future::BoxFuture;

use crate::FusionProviderAnswer;
use crate::FusionProviderRequest;

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct FusionProviderError {
    code: String,
    message: String,
}

impl FusionProviderError {
    #[must_use]
    pub fn new(code: impl Into<String>, message: impl Into<String>) -> Self {
        Self {
            code: code.into(),
            message: message.into(),
        }
    }

    #[must_use]
    pub fn code(&self) -> &str {
        &self.code
    }

    #[must_use]
    pub fn message(&self) -> &str {
        &self.message
    }
}

impl fmt::Display for FusionProviderError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "{}: {}", self.code, self.message)
    }
}

impl std::error::Error for FusionProviderError {}

pub trait FusionProvider: fmt::Debug + Send + Sync {
    fn complete(
        &self,
        request: FusionProviderRequest,
    ) -> BoxFuture<'static, Result<FusionProviderAnswer, FusionProviderError>>;
}

pub trait FusionProviderRouter: fmt::Debug + Send + Sync {
    fn resolve(&self, provider: &str) -> Option<Arc<dyn FusionProvider>>;
}

#[derive(Debug, Default)]
pub struct MapFusionProviderRouter {
    routes: HashMap<String, Arc<dyn FusionProvider>>,
}

impl MapFusionProviderRouter {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    #[must_use]
    pub fn with(mut self, provider: impl Into<String>, adapter: Arc<dyn FusionProvider>) -> Self {
        self.routes.insert(provider.into(), adapter);
        self
    }
}

impl FusionProviderRouter for MapFusionProviderRouter {
    fn resolve(&self, provider: &str) -> Option<Arc<dyn FusionProvider>> {
        self.routes.get(provider).map(Arc::clone)
    }
}

#[must_use]
pub fn answer_from_frames(frames: &[String]) -> Option<serde_json::Value> {
    let mut text = String::new();
    let mut completed = false;
    let mut final_message = false;
    for frame in frames {
        if completed {
            return None;
        }
        let value: serde_json::Value = serde_json::from_str(frame).ok()?;
        match value.get("type")?.as_str()? {
            "error" | "tool_call_input_delta" => return None,
            "completed" => {
                if !matches!(
                    value.get("endTurn"),
                    None | Some(serde_json::Value::Null | serde_json::Value::Bool(true))
                ) {
                    return None;
                }
                completed = true;
            }
            "output_item_added" | "output_item_done" => {
                let item = value.get("item")?;
                match item.get("type")?.as_str()? {
                    "reasoning" => continue,
                    "message" => {}
                    _ => return None,
                }
                if value.get("type")?.as_str()? != "output_item_done" {
                    continue;
                }
                if item.get("role")?.as_str()? != "assistant" {
                    return None;
                }
                match item.get("phase") {
                    None | Some(serde_json::Value::Null) => {}
                    Some(serde_json::Value::String(phase)) if phase == "final_answer" => {}
                    Some(serde_json::Value::String(phase)) if phase == "commentary" => continue,
                    _ => return None,
                }
                if final_message {
                    return None;
                }
                final_message = true;
                for part in item.get("content")?.as_array()? {
                    if part.get("type")?.as_str()? != "output_text" {
                        return None;
                    }
                    text.push_str(part.get("text")?.as_str()?);
                }
            }
            _ => {}
        }
    }
    let answer: serde_json::Value = serde_json::from_str(&text).ok()?;
    (completed && answer.is_object()).then_some(answer)
}
