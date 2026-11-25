//! Domain models and Cap'n Proto generated code.

pub mod app_state;
pub mod pending_operation;
pub mod totp;

/// Auto-generated Cap'n Proto bindings produced by `build.rs`.
#[allow(clippy::all, clippy::pedantic, clippy::nursery)]
pub mod generated {
    include!(concat!(env!("OUT_DIR"), "/mod.rs"));
}
