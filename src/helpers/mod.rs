//! Shared helper utilities: async primitives, crypto, and serialization.
//! See also: docs/concurrency.md.
//!
//! UI-facing time formatting lives in `components::shared::time_format`.

// Top-level modules
pub mod async_utils;
pub mod capnp;
pub mod concurrency;
pub mod encryption;
pub mod framing;
pub mod store_lock;

// Re-exports
pub use async_utils::{spawn, spawn_blocking};
