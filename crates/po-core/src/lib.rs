//! po-core — the platform-agnostic AI harness behind the po desktop app.
//!
//! This crate deliberately contains no GUI dependencies: it owns the session
//! log, the explicit agent state machine, the tool registry, the slash-command
//! registry, configuration, and provider adapters. The GPUI layer in
//! `po-ui`/`po-app` is a reactive projection of this state.

pub mod agent;
pub mod commands;
pub mod config;
pub mod model;
pub mod provider;
pub mod session;
pub mod tools;
