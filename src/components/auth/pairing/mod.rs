//! Device pairing UI.
//!
//! - **Share** (logged in): generates a QR code + pairing PIN.
//! - **Pair** (not logged in): scan/paste data, enter PIN + password.

mod actions;
mod pair_mode;
mod share_mode;

use crate::{
    components::shared::{CenteredPage, OrDividerLink},
    session::phase::SESSION,
};
use dioxus::prelude::{ReadableExt, *};

/// Pairing page shown in the auth flow for new-device login,
/// or from the account page on an existing device.
#[component]
pub fn Pairing() -> Element {
    let is_logged_in = use_memo(move || SESSION.read().authenticated_user().is_some());

    rsx! {
        CenteredPage {
            contained: is_logged_in(),
            width: "max-w-xs",
            fieldset {
                class: "fieldset w-full max-w-xs mx-auto gap-2 my-12",
                {
                    #[cfg(any(target_os = "android", target_os = "ios", target_os = "macos"))]
                    if crate::platform::native_bridge::local_network_granted() == Some(false) {
                        rsx! {
                            p {
                                class: "text-error text-sm text-center",
                                role: "alert",
                                "Local network access is denied, so this device cannot reach the "
                                "device you are pairing with. Allow it in system settings, then try again."
                            }
                        }
                    } else {
                        rsx! {}
                    }
                    #[cfg(not(any(target_os = "android", target_os = "ios", target_os = "macos")))]
                    rsx! {}
                }
                if is_logged_in() {
                    share_mode::ShareModeSection {}
                } else {
                    pair_mode::PairModeSection {}
                }
                if is_logged_in() {
                    OrDividerLink { href: "/account", label: "Back to account" }
                } else {
                    OrDividerLink { href: "/", label: "Back to login" }
                }
            }
        }
    }
}
