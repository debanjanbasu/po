//! po-core — the platform-agnostic AI harness behind the po desktop app.
//!
//! This crate deliberately contains no GUI dependencies: it owns the session
//! log, the explicit agent state machine, the tool registry, the slash-command
//! registry, configuration, and provider adapters. The GPUI layer in
//! `po-ui`/`po-app` is a reactive projection of this state.

pub mod agent;
pub mod app_mode;
pub mod auth;
pub mod commands;
pub mod config;
pub mod model;
pub mod paths;
pub mod provider;

pub use provider::{ChatProvider, MockEchoProvider, NanoServeProvider, ProviderError};
pub mod session;
pub mod session_store;
pub mod tools;

#[cfg(all(target_os = "macos", feature = "local-engine"))]
pub mod nanoai;

/// Format a transcript as a model prompt (delegates to the nano-ai adapter).
#[cfg(all(target_os = "macos", feature = "local-engine"))]
#[must_use]
pub fn nanoai_prompt(messages: &[model::ChatMessage]) -> String {
    nanoai::format_prompt(messages)
}

/// Format a transcript as a model prompt (plain fallback when the local
/// engine is not compiled in; used by the UI echo path and tests).
#[cfg(not(all(target_os = "macos", feature = "local-engine")))]
#[must_use]
pub fn nanoai_prompt(messages: &[model::ChatMessage]) -> String {
    messages
        .iter()
        .map(|m| format!("{}: {}", m.role.role_label(), m.content.trim()))
        .collect::<Vec<_>>()
        .join("\n")
}
