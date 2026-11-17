//! The explicit agent state machine.
//!
//! One reducer owns the lifecycle of a conversation. The UI is a reactive
//! projection of this state, and every entry point (chat slash command,
//! command palette, mouse click) dispatches the same events — which is what
//! makes "drive every feature from chat" a structural property rather than a
//! bolted-on feature.

use serde::{Deserialize, Serialize};

use crate::model::{ChatMessage, ToolCall};

/// The lifecycle state of the agent.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "state", rename_all = "kebab-case")]
pub enum AgentState {
    /// Nothing is running; the next user message starts a turn.
    Idle,
    /// A turn is running and the model is producing output.
    Streaming {
        /// Monotonic turn counter.
        turn: u32,
        /// Text generated so far.
        buffer: String,
    },
    /// The model asked for a tool; the harness is waiting for approval.
    AwaitingToolApproval {
        /// Monotonic turn counter.
        turn: u32,
        /// The pending call.
        call: ToolCall,
    },
    /// A tool is executing.
    RunningTool {
        /// Monotonic turn counter.
        turn: u32,
        /// Name of the running tool.
        tool: String,
    },
    /// The context is being compacted.
    Compacting {
        /// Monotonic turn counter.
        turn: u32,
    },
    /// The agent was stopped by the user.
    Stopped,
    /// The last turn ended in an error.
    Failed {
        /// Error description.
        message: String,
    },
}

impl AgentState {
    /// Whether a turn is currently in flight.
    #[must_use]
    pub fn is_busy(&self) -> bool {
        matches!(
            self,
            Self::Streaming { .. }
                | Self::AwaitingToolApproval { .. }
                | Self::RunningTool { .. }
                | Self::Compacting { .. }
        )
    }

    /// Whether the agent is waiting for the user to approve a tool call.
    #[must_use]
    pub fn awaiting_approval(&self) -> bool {
        matches!(self, Self::AwaitingToolApproval { .. })
    }
}

/// Events accepted by the reducer.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "event", rename_all = "kebab-case")]
pub enum AgentEvent {
    /// The user sent a message.
    UserMessage {
        /// Message text.
        content: String,
    },
    /// The model streamed a chunk.
    StreamDelta {
        /// The chunk.
        delta: String,
    },
    /// The model finished its turn.
    TurnComplete {
        /// The complete assistant message.
        message: ChatMessage,
    },
    /// The model requested a tool call.
    ToolCallRequested {
        /// The requested call.
        call: ToolCall,
    },
    /// The user approved a pending tool call.
    ToolApproved,
    /// The user denied a pending tool call.
    ToolDenied {
        /// Optional reason shown in the transcript.
        reason: Option<String>,
    },
    /// A tool finished executing.
    ToolFinished {
        /// Name of the tool that ran.
        tool: String,
    },
    /// The user asked to abort the current turn.
    Abort,
    /// The user asked to compact the context.
    Compact,
    /// Compaction finished.
    Compacted {
        /// Summary of the compacted prefix.
        summary: String,
    },
    /// The turn failed.
    Failed {
        /// Error description.
        message: String,
    },
    /// The agent was stopped.
    Stop,
    /// The agent was restarted after a stop.
    Resume,

    // ---- commands (chat, palette, and buttons all land here) -----------
    /// Show or switch the active model.
    SetModel {
        /// Model or provider name; empty means "show the current one".
        model: String,
    },
    /// Start a new session.
    SessionNew,
    /// List stored sessions.
    SessionList,
    /// Switch to another session.
    SessionSwitch {
        /// Target session id.
        id: String,
    },
    /// Fork the current session.
    SessionFork {
        /// Optional target id; empty means "fork the current session".
        id: String,
    },
    /// Set the colour theme.
    SetTheme {
        /// `light`, `dark`, or `toggle`.
        mode: String,
    },
    /// Set the UI font family.
    SetFont {
        /// Font family name.
        family: String,
    },
    /// List the available tools.
    ListTools,
    /// Clear the transcript view.
    ClearView,
    /// Quit the application.
    Quit,
    /// The user typed something that looked like an unknown command.
    UnknownCommand {
        /// The raw input.
        input: String,
    },
}

/// Side effects the reducer asks the host to perform.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "effect", rename_all = "kebab-case")]
pub enum Effect {
    /// Start a provider turn with the given transcript.
    StartTurn {
        /// Messages to send to the model.
        messages: Vec<ChatMessage>,
    },
    /// Append text to the streaming buffer.
    AppendDelta {
        /// The chunk.
        delta: String,
    },
    /// Record the completed assistant message.
    RecordMessage {
        /// The message.
        message: ChatMessage,
    },
    /// Run the given tool.
    RunTool {
        /// The call to execute.
        call: ToolCall,
    },
    /// Record a tool result in the transcript.
    RecordToolResult {
        /// Tool name.
        tool: String,
        /// Result payload.
        result: String,
        /// Whether the tool succeeded.
        ok: bool,
    },
    /// Abort the in-flight provider request.
    AbortProvider,
    /// Compact the transcript.
    CompactContext,
    /// The agent is stopped.
    Stopped,

    // ---- command effects ----------------------------------------------
    /// Change the active model/provider.
    SetModel {
        /// Model or provider name.
        model: String,
    },
    /// Change the colour theme.
    SetTheme {
        /// Theme mode name.
        mode: String,
    },
    /// Change the UI font family.
    SetFont {
        /// Font family name.
        family: String,
    },
    /// Create a new session.
    NewSession,
    /// List stored sessions.
    ListSessions,
    /// Switch to another session.
    SwitchSession {
        /// Target session id.
        id: String,
    },
    /// Fork a session.
    ForkSession {
        /// Optional target id.
        id: String,
    },
    /// List the registered tools.
    ListTools,
    /// Clear the transcript view.
    ClearView,
    /// Quit the application.
    Quit,
    /// Report an unknown command to the user.
    UnknownCommand {
        /// The raw input.
        input: String,
    },
}

/// Result of applying one event.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Transition {
    /// The new state.
    pub state: AgentState,
    /// Effects to execute, in order.
    pub effects: Vec<Effect>,
}

/// Apply an event to a state.
///
/// The reducer is pure: it never performs IO, never touches the filesystem,
/// and never spawns work. Everything that must happen outside is returned as
/// an [`Effect`]. That is what makes the machine replayable and testable.
#[must_use]
pub fn transition(state: &AgentState, event: &AgentEvent) -> Transition {
    let mut effects = Vec::new();
    let next = match (state, event) {
        // ---- user input -------------------------------------------------
        (AgentState::Idle, AgentEvent::UserMessage { content }) => {
            let message = ChatMessage::user(content.clone());
            effects.push(Effect::StartTurn {
                messages: vec![message],
            });
            AgentState::Streaming {
                turn: 0,
                buffer: String::new(),
            }
        }
        (AgentState::Failed { .. }, AgentEvent::UserMessage { content }) => {
            let message = ChatMessage::user(content.clone());
            effects.push(Effect::StartTurn {
                messages: vec![message],
            });
            AgentState::Streaming {
                turn: 0,
                buffer: String::new(),
            }
        }
        (AgentState::Stopped, AgentEvent::UserMessage { .. }) => AgentState::Stopped,
        (busy, AgentEvent::UserMessage { .. }) if busy.is_busy() => busy.clone(),

        // ---- streaming --------------------------------------------------
        (
            AgentState::Streaming { turn, buffer },
            AgentEvent::StreamDelta { delta },
        ) => {
            let mut next_buffer = buffer.clone();
            next_buffer.push_str(delta);
            effects.push(Effect::AppendDelta {
                delta: delta.clone(),
            });
            AgentState::Streaming {
                turn: *turn,
                buffer: next_buffer,
            }
        }
        (
            AgentState::Streaming { turn, buffer },
            AgentEvent::TurnComplete { message },
        ) => {
            let mut recorded = message.clone();
            if recorded.content.is_empty() {
                recorded.content = buffer.clone();
            }
            effects.push(Effect::RecordMessage { message: recorded });
            AgentState::Idle
        }

        // ---- tools ------------------------------------------------------
        (
            AgentState::Streaming { turn, .. },
            AgentEvent::ToolCallRequested { call },
        ) => {
            AgentState::AwaitingToolApproval {
                turn: *turn,
                call: call.clone(),
            }
        }
        (
            AgentState::AwaitingToolApproval { turn, call },
            AgentEvent::ToolApproved,
        ) => {
            effects.push(Effect::RunTool { call: call.clone() });
            AgentState::RunningTool {
                turn: *turn,
                tool: call.name.clone(),
            }
        }
        (
            AgentState::AwaitingToolApproval { turn, call },
            AgentEvent::ToolDenied { reason },
        ) => {
            let result = reason.clone().unwrap_or_else(|| "denied by user".to_string());
            effects.push(Effect::RecordToolResult {
                tool: call.name.clone(),
                result,
                ok: false,
            });
            AgentState::Streaming {
                turn: *turn,
                buffer: String::new(),
            }
        }
        (
            AgentState::RunningTool { turn, tool },
            AgentEvent::ToolFinished { tool: finished },
        ) => {
            effects.push(Effect::RecordToolResult {
                tool: tool.clone(),
                result: format!("{finished} finished"),
                ok: true,
            });
            AgentState::Streaming {
                turn: *turn,
                buffer: String::new(),
            }
        }

        // ---- lifecycle --------------------------------------------------
        (state, AgentEvent::Abort) => {
            if state.is_busy() {
                effects.push(Effect::AbortProvider);
            }
            AgentState::Idle
        }
        (state, AgentEvent::Compact) => {
            if state.is_busy() {
                state.clone()
            } else {
                effects.push(Effect::CompactContext);
                AgentState::Compacting { turn: 0 }
            }
        }
        (AgentState::Compacting { turn }, AgentEvent::Compacted { summary }) => {
            let _ = turn;
            effects.push(Effect::RecordMessage {
                message: ChatMessage::system(format!("context compacted: {summary}")),
            });
            AgentState::Idle
        }
        (_, AgentEvent::Failed { message }) => AgentState::Failed {
            message: message.clone(),
        },
        (_, AgentEvent::Stop) => {
            effects.push(Effect::Stopped);
            AgentState::Stopped
        }
        (AgentState::Stopped, AgentEvent::Resume) => AgentState::Idle,
        (_, AgentEvent::Resume) => state.clone(),

        // ---- commands (chat, palette, buttons all land here) -----------
        (_, AgentEvent::SetModel { model }) => {
            effects.push(Effect::SetModel { model: model.clone() });
            state.clone()
        }
        (_, AgentEvent::SessionNew) => {
            effects.push(Effect::NewSession);
            AgentState::Idle
        }
        (_, AgentEvent::SessionList) => {
            effects.push(Effect::ListSessions);
            state.clone()
        }
        (_, AgentEvent::SessionSwitch { id }) => {
            effects.push(Effect::SwitchSession { id: id.clone() });
            AgentState::Idle
        }
        (_, AgentEvent::SessionFork { id }) => {
            effects.push(Effect::ForkSession { id: id.clone() });
            AgentState::Idle
        }
        (_, AgentEvent::SetTheme { mode }) => {
            effects.push(Effect::SetTheme { mode: mode.clone() });
            state.clone()
        }
        (_, AgentEvent::SetFont { family }) => {
            effects.push(Effect::SetFont { family: family.clone() });
            state.clone()
        }
        (_, AgentEvent::ListTools) => {
            effects.push(Effect::ListTools);
            state.clone()
        }
        (_, AgentEvent::ClearView) => {
            effects.push(Effect::ClearView);
            state.clone()
        }
        (_, AgentEvent::Quit) => {
            effects.push(Effect::Quit);
            AgentState::Stopped
        }
        (_, AgentEvent::UnknownCommand { input }) => {
            effects.push(Effect::UnknownCommand { input: input.clone() });
            state.clone()
        }

        // ---- invalid transitions ---------------------------------------
        (state, _) => state.clone(),
    };

    Transition {
        state: next,
        effects,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tool_call() -> ToolCall {
        ToolCall {
            id: "call-1".into(),
            name: "read_file".into(),
            arguments: serde_json::json!({"path": "/tmp/x"}),
        }
    }

    #[test]
    fn user_message_starts_a_turn() {
        let t = transition(&AgentState::Idle, &AgentEvent::UserMessage {
            content: "hello".into(),
        });
        assert!(matches!(t.state, AgentState::Streaming { .. }));
        assert_eq!(t.effects.len(), 1);
        assert!(matches!(t.effects[0], Effect::StartTurn { .. }));
    }

    #[test]
    fn deltas_accumulate_into_the_buffer() {
        let mut state = AgentState::Streaming {
            turn: 0,
            buffer: String::new(),
        };
        for delta in ["Hel", "lo", "!"] {
            let t = transition(
                &state,
                &AgentEvent::StreamDelta {
                    delta: delta.to_string(),
                },
            );
            state = t.state;
        }
        match state {
            AgentState::Streaming { buffer, .. } => assert_eq!(buffer, "Hello!"),
            other => panic!("unexpected state: {other:?}"),
        }
    }

    #[test]
    fn tool_call_requires_approval_before_running() {
        let streaming = AgentState::Streaming {
            turn: 1,
            buffer: String::new(),
        };
        let requested = transition(
            &streaming,
            &AgentEvent::ToolCallRequested { call: tool_call() },
        );
        assert!(requested.state.awaiting_approval());
        assert!(requested.effects.is_empty(), "must not run before approval");

        let approved = transition(&requested.state, &AgentEvent::ToolApproved);
        assert!(matches!(approved.state, AgentState::RunningTool { .. }));
        assert!(matches!(approved.effects[0], Effect::RunTool { .. }));
    }

    #[test]
    fn denial_records_a_failed_tool_result() {
        let waiting = AgentState::AwaitingToolApproval {
            turn: 1,
            call: tool_call(),
        };
        let t = transition(
            &waiting,
            &AgentEvent::ToolDenied {
                reason: None,
            },
        );
        assert!(matches!(t.state, AgentState::Streaming { .. }));
        match &t.effects[0] {
            Effect::RecordToolResult { ok, .. } => assert!(!ok),
            other => panic!("unexpected effect: {other:?}"),
        }
    }

    #[test]
    fn a_second_user_message_is_ignored_while_busy() {
        let busy = AgentState::Streaming {
            turn: 1,
            buffer: "x".into(),
        };
        let t = transition(
            &busy,
            &AgentEvent::UserMessage {
                content: "second".into(),
            },
        );
        assert_eq!(t.state, busy);
        assert!(t.effects.is_empty());
    }

    #[test]
    fn abort_returns_to_idle_and_signals_the_provider() {
        let busy = AgentState::Streaming {
            turn: 2,
            buffer: "partial".into(),
        };
        let t = transition(&busy, &AgentEvent::Abort);
        assert_eq!(t.state, AgentState::Idle);
        assert_eq!(t.effects, vec![Effect::AbortProvider]);
    }

    #[test]
    fn stop_and_resume_are_explicit() {
        let stopped = transition(&AgentState::Idle, &AgentEvent::Stop);
        assert_eq!(stopped.state, AgentState::Stopped);
        assert_eq!(stopped.effects, vec![Effect::Stopped]);

        let resumed = transition(&stopped.state, &AgentEvent::Resume);
        assert_eq!(resumed.state, AgentState::Idle);
    }

    #[test]
    fn compaction_is_refused_while_busy() {
        let busy = AgentState::Streaming {
            turn: 0,
            buffer: String::new(),
        };
        let t = transition(&busy, &AgentEvent::Compact);
        assert_eq!(t.state, busy);
        assert!(t.effects.is_empty());
    }
}
