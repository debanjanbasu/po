//! gpui UI layer (Dioxus → gpui-ce migration in progress).
//!
//! New modules land here; the Dioxus `src/components` tree keeps building in
//! parallel until the Phase 7 cutover deletes it.
//!
//! `allow(dead_code)` is scoped to this tree ON PURPOSE (mirrors
//! `platform/native_bridge`): the screens, widgets, and state APIs are
//! written ahead of their mount points — `RootView` mounts placeholders
//! until the session cutover wires the real screens in. Each item here gets
//! consumed at the cutover; this allow is deleted then (AGENTS dead-code
//! policy: wire it up or delete it — "wire up" happens at the cutover).
#![allow(dead_code)]

pub mod app;
pub mod assets;
pub mod auth;
pub mod boot;
pub mod chat;
pub mod nav_item;
pub mod router;
pub mod screens;
pub mod startup;
pub mod state;
pub mod theme;
pub mod widgets;
