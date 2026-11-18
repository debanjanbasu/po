//! Core data model for po's chat harness.
//!
//! These types are intentionally plain: they are serialized into the
//! append-only session log, so they must stay stable and UI-agnostic.

use serde::{Deserialize, Serialize};

/// Which backend produces completions for a conversation.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "kebab-case")]
pub enum Provider {
    /// The bundled nano-ai engine running a local model (Apple Silicon only).
    LocalNano {
        /// Filesystem path to the model weights.
        model_path: String,
    },
}

impl Provider {
    /// Human-readable label shown in the status bar.
    #[must_use]
    pub fn label(&self) -> String {
        match self {
            Self::LocalNano { model_path } => {
                let name = std::path::Path::new(model_path)
                    .file_name()
                    .and_then(|n| n.to_str())
                    .unwrap_or("local");
                format!("nano-ai: {name}")
            }
        }
    }

    /// Whether the provider streams tokens incrementally.
    #[must_use]
    pub fn supports_streaming(&self) -> bool {
        true
    }
}

/// Author of a chat message.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ChatRole {
    /// Typed by the human.
    User,
    /// Produced by the model.
    Assistant,
    /// Produced by the harness (notices, errors, command output).
    System,
    /// A tool invocation.
    Tool,
}

impl ChatRole {
    /// Lowercase label used in plain-text prompts and logs.
    #[must_use]
    pub fn role_label(self) -> &'static str {
        match self {
            Self::User => "user",
            Self::Assistant => "assistant",
            Self::System => "system",
            Self::Tool => "tool",
        }
    }
}

/// A tool the model asked to run.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ToolCall {
    /// Stable identifier used to correlate the call with its result.
    pub id: String,
    /// Registered tool name, e.g. `read_file`.
    pub name: String,
    /// Raw JSON arguments as supplied by the model.
    pub arguments: serde_json::Value,
}

/// The outcome of a tool call.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ToolResult {
    /// Identifier of the originating [`ToolCall`].
    pub id: String,
    /// Whether the tool succeeded.
    pub ok: bool,
    /// Human-readable summary shown in the transcript.
    pub summary: String,
    /// Full output, kept for the model and for replay.
    #[serde(default)]
    pub content: String,
    /// Unified diff, when the tool produced one (edits).
    #[serde(default)]
    pub diff: Option<String>,
}

/// One entry in the conversation transcript.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ChatMessage {
    /// Who produced the message.
    pub role: ChatRole,
    /// Message body.
    pub content: String,
    /// Tool call requested with this message, if any.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tool_call: Option<ToolCall>,
    /// Tool result attached to this message, if any.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tool_result: Option<ToolResult>,
}

impl ChatMessage {
    /// Create a plain message.
    #[must_use]
    pub fn new(role: ChatRole, content: impl Into<String>) -> Self {
        Self {
            role,
            content: content.into(),
            tool_call: None,
            tool_result: None,
        }
    }

    /// Create a user message.
    #[must_use]
    pub fn user(content: impl Into<String>) -> Self {
        Self::new(ChatRole::User, content)
    }

    /// Create an assistant message.
    #[must_use]
    pub fn assistant(content: impl Into<String>) -> Self {
        Self::new(ChatRole::Assistant, content)
    }

    /// Create a system message.
    #[must_use]
    pub fn system(content: impl Into<String>) -> Self {
        Self::new(ChatRole::System, content)
    }
}

/// Incremental events emitted while a turn is running.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "event", rename_all = "kebab-case")]
pub enum StreamEvent {
    /// A chunk of generated text.
    Delta(String),
    /// The model finished generating.
    Done {
        /// The complete assistant message.
        message: ChatMessage,
    },
    /// The model requested a tool call.
    ToolCall(ToolCall),
    /// The turn failed.
    Error(String),
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn provider_label_is_human_readable() {
        let local = Provider::LocalNano {
            model_path: "/models/minicpm-v-4.7-1b/minicpm-v-4.7-1b-tern4.bin".into(),
        };
        assert!(local.label().contains("nano-ai"));
    }
}
