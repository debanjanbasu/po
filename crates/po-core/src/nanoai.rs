//! Adapter for the local nano-ai engine.
//!
//! `nano-engine` is consumed straight from the nano-ai repository, pinned
//! to a commit. Because we own nano-ai, this adapter is deliberately thin:
//! it owns the prompt format, the streaming bridge, and the translation from
//! engine pieces into po's [`StreamEvent`]s.

use std::path::Path;

use nano_engine::model::{AsyncEngineHandle, GenerationReport};
use nano_engine::sampler::SamplingParams;
use tokio::sync::mpsc::UnboundedSender;

use crate::model::{ChatMessage, ChatRole, Provider, StreamEvent};

/// Errors raised by the local engine adapter.
#[derive(Debug, thiserror::Error)]
pub enum EngineError {
    /// The engine refused to load the model.
    #[error("nano-ai engine error: {0}")]
    Engine(#[from] nano_engine::Error),
    /// The model file could not be read.
    #[error("could not read model at {0}: {1}")]
    Io(std::path::PathBuf, #[source] std::io::Error),
    /// The engine's worker thread went away.
    #[error("nano-ai engine worker stopped")]
    WorkerGone,
}

/// A running local engine.
#[derive(Clone)]
pub struct LocalEngine {
    handle: AsyncEngineHandle,
}

impl LocalEngine {
    /// Load a model and start the engine worker.
    ///
    /// # Errors
    /// Returns an error when the model file is missing or cannot be loaded.
    pub async fn open(model_path: impl AsRef<Path>) -> Result<Self, EngineError> {
        let path = model_path.as_ref();
        if !path.exists() {
            return Err(EngineError::Io(
                path.to_path_buf(),
                std::io::Error::new(std::io::ErrorKind::NotFound, "model file not found"),
            ));
        }
        let handle = AsyncEngineHandle::open(path).await?;
        Ok(Self { handle })
    }

    /// Name of the device the engine runs on.
    #[must_use]
    pub fn device_name(&self) -> &str {
        self.handle.device_name()
    }

    /// Generate a completion, streaming pieces into `pieces` as they arrive.
    ///
    /// # Errors
    /// Returns an error when generation fails.
    pub async fn generate(
        &self,
        prompt: &str,
        max_new_tokens: usize,
        pieces: Option<UnboundedSender<String>>,
    ) -> Result<GenerationReport, EngineError> {
        let report = self
            .handle
            .generate_text(
                prompt.to_string(),
                max_new_tokens,
                SamplingParams::default(),
                pieces,
            )
            .await?;
        Ok(report)
    }

    /// Generate a completion for a transcript, using nano-ai's chat format.
    ///
    /// # Errors
    /// Returns an error when generation fails.
    pub async fn chat(
        &self,
        messages: &[ChatMessage],
        max_new_tokens: usize,
        pieces: Option<UnboundedSender<String>>,
    ) -> Result<GenerationReport, EngineError> {
        let prompt = format_prompt(messages);
        self.generate(&prompt, max_new_tokens, pieces).await
    }
}

/// Format a transcript as the plain-text prompt nano-ai expects.
///
/// The format mirrors the roles used by the local model's chat template:
/// `user:` for the human, `assistant:` for the model, `system:` for notices.
#[must_use]
pub fn format_prompt(messages: &[ChatMessage]) -> String {
    let mut prompt = String::new();
    for message in messages {
        let role = match message.role {
            ChatRole::User => "user",
            ChatRole::Assistant => "assistant",
            ChatRole::System => "system",
            ChatRole::Tool => "tool",
        };
        prompt.push_str(role);
        prompt.push_str(": ");
        prompt.push_str(message.content.trim());
        prompt.push('\n');
    }
    prompt.push_str("assistant: ");
    prompt
}

/// Translate an engine piece into a po stream event.
#[must_use]
pub fn piece_to_event(piece: String) -> StreamEvent {
    StreamEvent::Delta(piece)
}

/// Build the provider description for a local model path.
#[must_use]
pub fn local_provider(model_path: impl Into<String>) -> Provider {
    Provider::LocalNano {
        model_path: model_path.into(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn prompt_formatting_uses_role_labels() {
        let messages = vec![
            ChatMessage::system("be brief"),
            ChatMessage::user("hello"),
            ChatMessage::assistant("hi"),
        ];
        let prompt = format_prompt(&messages);
        assert!(prompt.contains("system: be brief"));
        assert!(prompt.contains("user: hello"));
        assert!(prompt.contains("assistant: hi"));
        assert!(prompt.ends_with("assistant: "));
    }

    #[test]
    fn pieces_become_delta_events() {
        let event = piece_to_event("tok".into());
        assert_eq!(event, StreamEvent::Delta("tok".into()));
    }

    #[test]
    fn local_provider_carries_the_model_path() {
        let provider = local_provider("/models/m.bin");
        assert!(provider.label().contains("m.bin"));
    }
}
