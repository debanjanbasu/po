//! UI component modules. Each sub-module is a Dioxus page or reusable widget.

mod auth;
mod features;
pub mod overlays;
mod settings;
pub mod shared;

pub use {
    auth::{BiometricGate, LoginSignup, Pairing, PinGate, RestoreSplash},
    features::chat::{Chat, ChatConversation},
    features::totp::Totp,
    features::{Ai, Calendar, Calls},
    overlays::{ActivityBar, ActivityModal, ErrorToast, PostRegister},
    settings::account::Account,
    settings::profile::Profile,
    settings::security::Security,
    settings::snapshots::Snapshots,
    shared::{PageNotFound, WindowChrome},
};
