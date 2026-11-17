//! Slash-command registry.
//!
//! Every command here is reachable three ways: typed in the chat as
//! `/command`, picked from the command palette, or triggered by a UI button.
//! All three funnel into [`CommandRegistry::event`], which produces the same
//! [`AgentEvent`] — so the chat can drive every feature of the app.

use serde::{Deserialize, Serialize};

use crate::agent::AgentEvent;

/// A command exposed to the user.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Command {
    /// Stable identifier, also used as the palette key.
    pub id: String,
    /// The slash form, without arguments.
    pub slash: String,
    /// One-line description shown in the palette.
    pub description: String,
}

/// A parsed command invocation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Invocation {
    /// The matched command.
    pub id: String,
    /// Remaining text after the command word.
    pub args: String,
}

/// Registry of all slash commands.
#[derive(Debug, Clone)]
pub struct CommandRegistry {
    commands: Vec<Command>,
}

impl Default for CommandRegistry {
    fn default() -> Self {
        Self::new()
    }
}

impl CommandRegistry {
    /// Build the registry with po's built-in commands.
    #[must_use]
    pub fn new() -> Self {
        let commands = vec![
            Command {
                id: "model".into(),
                slash: "/model".into(),
                description: "Show or switch the active model/provider".into(),
            },
            Command {
                id: "session.new".into(),
                slash: "/session new".into(),
                description: "Start a new session".into(),
            },
            Command {
                id: "session.list".into(),
                slash: "/session list".into(),
                description: "List saved sessions".into(),
            },
            Command {
                id: "session.switch".into(),
                slash: "/session switch".into(),
                description: "Switch to a session by id".into(),
            },
            Command {
                id: "session.fork".into(),
                slash: "/session fork".into(),
                description: "Fork the current session into a new branch".into(),
            },
            Command {
                id: "compact".into(),
                slash: "/compact".into(),
                description: "Compact the conversation context".into(),
            },
            Command {
                id: "theme".into(),
                slash: "/theme".into(),
                description: "Toggle or set the colour theme (light|dark|toggle)".into(),
            },
            Command {
                id: "font".into(),
                slash: "/font".into(),
                description: "Set the UI font family".into(),
            },
            Command {
                id: "tools".into(),
                slash: "/tools".into(),
                description: "List available tools".into(),
            },
            Command {
                id: "clear".into(),
                slash: "/clear".into(),
                description: "Clear the current transcript view".into(),
            },
            Command {
                id: "quit".into(),
                slash: "/quit".into(),
                description: "Quit po".into(),
            },
        ];
        Self { commands }
    }

    /// All registered commands.
    #[must_use]
    pub fn commands(&self) -> &[Command] {
        &self.commands
    }

    /// Look up a command by its stable id.
    #[must_use]
    pub fn get(&self, id: &str) -> Option<&Command> {
        self.commands.iter().find(|c| c.id == id)
    }

    /// Parse a chat line into a command invocation.
    ///
    /// Returns `None` when the line is not a slash command or names an unknown
    /// command.
    #[must_use]
    pub fn parse(&self, line: &str) -> Option<Invocation> {
        let trimmed = line.trim();
        let body = trimmed.strip_prefix('/')?;
        let mut parts = body.splitn(2, char::is_whitespace);
        let head = parts.next().unwrap_or_default().trim();
        if head.is_empty() {
            return None;
        }
        let args = parts.next().unwrap_or_default().trim().to_string();

        // A multi-word slash form such as `/session new` is matched first;
        // otherwise the first word is matched as a top-level command.
        if let Some(command) = self
            .commands
            .iter()
            .find(|c| c.slash.eq_ignore_ascii_case(trimmed))
        {
            return Some(Invocation {
                id: command.id.clone(),
                args: String::new(),
            });
        }

        let head_lower = head.to_ascii_lowercase();
        let (id, rest) = match head_lower.as_str() {
            "session" => {
                let sub = rest_first_word(&args);
                match sub.as_deref() {
                    Some("new") => ("session.new", strip_word(&args, "new")),
                    Some("list") => ("session.list", strip_word(&args, "list")),
                    Some("switch") => ("session.switch", strip_word(&args, "switch")),
                    Some("fork") => ("session.fork", strip_word(&args, "fork")),
                    _ => return None,
                }
            }
            other => (other, args.clone()),
        };

        let command = self.get(id)?;
        Some(Invocation {
            id: command.id.clone(),
            args: rest,
        })
    }

    /// Map a command id (and its arguments) onto an agent event.
    ///
    /// # Errors
    /// Returns an error when the id is unknown.
    pub fn event(&self, id: &str, args: &str) -> Result<AgentEvent, UnknownCommand> {
        let event = match id {
            "model" => AgentEvent::SetModel {
                model: args.trim().to_string(),
            },
            "session.new" => AgentEvent::SessionNew,
            "session.list" => AgentEvent::SessionList,
            "session.switch" => AgentEvent::SessionSwitch {
                id: args.trim().to_string(),
            },
            "session.fork" => AgentEvent::SessionFork {
                id: args.trim().to_string(),
            },
            "compact" => AgentEvent::Compact,
            "theme" => AgentEvent::SetTheme {
                mode: args.trim().to_string(),
            },
            "font" => AgentEvent::SetFont {
                family: args.trim().to_string(),
            },
            "tools" => AgentEvent::ListTools,
            "clear" => AgentEvent::ClearView,
            "quit" => AgentEvent::Quit,
            other => return Err(UnknownCommand(other.to_string())),
        };
        Ok(event)
    }
}

/// Error returned when a command id is not registered.
#[derive(Debug, thiserror::Error)]
#[error("unknown command: {0}")]
pub struct UnknownCommand(pub String);

fn rest_first_word(args: &str) -> Option<String> {
    args.split_whitespace().next().map(str::to_ascii_lowercase)
}

fn strip_word(args: &str, word: &str) -> String {
    let trimmed = args.trim_start();
    if trimmed.len() >= word.len()
        && trimmed[..word.len()].eq_ignore_ascii_case(word)
    {
        trimmed[word.len()..].trim().to_string()
    } else {
        trimmed.to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn registry_contains_every_planned_command() {
        let registry = CommandRegistry::new();
        for id in [
            "model",
            "session.new",
            "session.list",
            "session.switch",
            "session.fork",
            "compact",
            "theme",
            "font",
            "tools",
            "clear",
            "quit",
        ] {
            assert!(registry.get(id).is_some(), "missing command {id}");
        }
    }

    #[test]
    fn parses_plain_slash_commands() {
        let registry = CommandRegistry::new();
        let invocation = registry.parse("/compact").expect("parsed");
        assert_eq!(invocation.id, "compact");
        assert!(invocation.args.is_empty());
    }

    #[test]
    fn parses_subcommands_with_arguments() {
        let registry = CommandRegistry::new();
        let invocation = registry.parse("/session switch abc123").expect("parsed");
        assert_eq!(invocation.id, "session.switch");
        assert_eq!(invocation.args, "abc123");
    }

    #[test]
    fn rejects_non_commands_and_unknown_commands() {
        let registry = CommandRegistry::new();
        assert!(registry.parse("hello world").is_none());
        assert!(registry.parse("/nope").is_none());
        assert!(registry.parse("/session bogus").is_none());
    }

    #[test]
    fn command_ids_map_to_agent_events() {
        let registry = CommandRegistry::new();
        assert_eq!(
            registry.event("session.list", "").expect("event"),
            AgentEvent::SessionList
        );
        assert_eq!(
            registry.event("theme", "dark").expect("event"),
            AgentEvent::SetTheme {
                mode: "dark".into()
            }
        );
        assert!(registry.event("bogus", "").is_err());
    }
}
