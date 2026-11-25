//! Overlay components: toasts, banners, modals, post-registration.
//! See also: docs/debugging.md.

use dioxus::prelude::*;

// `deep_link_toast` stays a public module: session::phase (outside
// `components`) consumes its items by full path.
pub mod deep_link_toast;

mod activity_bar;
mod activity_modal;
mod activity_panels;
mod activity_timeline_row;
mod error_toast;
mod post_register;

pub use activity_bar::{ActivityBar, TOP_STRIP_PADDING};
pub use activity_modal::ActivityModal;
pub use error_toast::ErrorToast;
pub use post_register::PostRegister;

pub(super) static SHOW_ACTIVITY_MODAL: GlobalSignal<bool> = Signal::global(|| false);
