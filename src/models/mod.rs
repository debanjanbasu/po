//! Domain models and Cap'n Proto generated code.

pub mod app_state;
pub mod pending_operation;
pub mod totp;

/// Auto-generated Cap'n Proto bindings produced by `build.rs`.
// The generated Cap'n Proto modules self-allow `unused`/`unsafe_code`; the
// pedantic lints that still fire on their output (module_inception,
// elidable_lifetime_names, unreadable_literal, use_self) are allow-listed
// exactly — generated code is not hand-fixable here.
#[allow(
    clippy::module_inception,
    clippy::elidable_lifetime_names,
    clippy::unreadable_literal,
    clippy::use_self
)]
pub mod generated {
    include!(concat!(env!("OUT_DIR"), "/mod.rs"));
}
