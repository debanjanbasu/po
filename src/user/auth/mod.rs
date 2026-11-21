//! Authentication flows: signup and credential checks, second-factor
//! admission, onboarding/profile updates, remembered sessions, keypair
//! crypto, and logout/account-deletion lifecycle.
//!
//! Owns: none — see docs/concurrency.md for session-phase ownership.

mod keypair_crypto;
mod lifecycle;
pub mod login_signup;
mod onboarding;
pub mod remember;

pub(super) use keypair_crypto::{decrypt_keypair, decrypt_keypair_with_secret, encrypt_keypair};
pub use lifecycle::admit_user;
pub(super) use login_signup::login_with_password;
pub use login_signup::{hydrate_remembered_user, signup};
pub use onboarding::update_onboarding;
pub(super) use remember::{
    RememberedSessionEffect, apply_pending_local_auth, apply_remembered_session_effect,
    build_remembered_session_effect, clear_pending_local_auth, take_matching_staged,
};
